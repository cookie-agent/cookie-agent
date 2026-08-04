use super::*;

impl Engine {
    pub async fn approval_respond(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult, EngineError> {
        let executor = self
            .inner
            .pending_approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(params.session_id, params.approval_id))
            .map(|pending| pending.executor.clone());
        let Some(executor) = executor else {
            return self
                .request(params.session_id, |reply| SessionCommand::ApprovalRespond {
                    params,
                    reply,
                })
                .await;
        };

        let guard = executor.lock_owned().await;
        let invalidation = match guard.as_ref() {
            Some(executor) => executor.revalidate().await.err().map(|error| match error {
                ToolError::OperationChanged(_) => PreparedApprovalInvalidation::OperationChanged,
                _ => PreparedApprovalInvalidation::PreparedCapabilityLost,
            }),
            None => Some(PreparedApprovalInvalidation::PreparedCapabilityLost),
        };
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
