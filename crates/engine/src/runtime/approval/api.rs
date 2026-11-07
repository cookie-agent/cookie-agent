use cookie_agent_protocol::{
    ApprovalListResult, ApprovalRespondParams, ApprovalRespondResult, ApprovalStatus, EventOrigin,
    SessionId,
};

use crate::runtime::{
    Engine, EngineError, PreparedApprovalInvalidation, approval::projection::approval_records,
    helpers::root_id,
};
use cookie_agent_runtime_api::ToolError;

impl Engine {
    pub async fn approval_respond(
        &self,
        params: ApprovalRespondParams,
        _origin: EventOrigin,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let _permission_guard = self
            .inner
            .approvals
            .permission_overlay_mutation
            .lock()
            .await;
        let pending = self
            .inner
            .approvals
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(params.session_id, params.approval_id))
            .map(|pending| (pending.executor.clone(), pending.permission_overlay_epoch));
        let Some((executor, pending_epoch)) = pending else {
            return self
                .on_actor(params.session_id, move |engine| {
                    engine.approval_respond_direct(params)
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
                != self
                    .inner
                    .store
                    .get(params.session_id)?
                    .log
                    .state()
                    .permission_overlay_epoch()
        {
            invalidation = Some(PreparedApprovalInvalidation::OperationChanged);
        }
        if let Some(invalidation) = invalidation {
            return self
                .on_actor(params.session_id, move |engine| {
                    engine.approval_capability_invalid_direct(params, invalidation)
                })
                .await;
        }
        self.on_actor(params.session_id, move |engine| {
            engine.approval_respond_direct(params)
        })
        .await
    }

    pub fn list_approvals(
        &self,
        root_session_id: SessionId,
        status: Option<ApprovalStatus>,
    ) -> Result<ApprovalListResult, EngineError> {
        let sessions = self
            .inner
            .store
            .all()
            .into_iter()
            .filter(|session| {
                root_id(&session.meta.origin, session.meta.session_id) == root_session_id
            })
            .collect::<Vec<_>>();
        let mut approvals = Vec::new();
        for session in sessions {
            // What is open is folded; anything else takes a pass over the log.
            match status {
                Some(ApprovalStatus::Pending | ApprovalStatus::Escalated) => approvals.extend(
                    session
                        .log
                        .state()
                        .open_approvals()
                        .map(|open| open.record.clone()),
                ),
                _ => approvals
                    .extend(approval_records(session.meta.session_id, &session.log)?.into_values()),
            }
        }
        approvals.retain(|record| status.is_none_or(|status| record.status == status));
        Ok(ApprovalListResult {
            approvals,
            tree_grants: self.inner.approvals.store.for_root(root_session_id),
        })
    }
}
