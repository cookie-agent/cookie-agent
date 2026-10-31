//! Test seams of `runtime/admission.rs`.

use crate::runtime::*;

impl crate::runtime::Engine {
    pub(crate) fn cancel_inflight_delegation_for_test(
        &self,
        invocation_id: InvocationId,
    ) -> Result<(), EngineError> {
        let mut admissions = self
            .inner
            .delegation
            .inflight
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let entries = admissions
            .get_mut(&invocation_id)
            .ok_or_else(|| EngineError::ToolFailed("delegate admission is not in flight".into()))?;
        for admission in entries.values_mut() {
            admission.cancelled = true;
        }
        Ok(())
    }
}
