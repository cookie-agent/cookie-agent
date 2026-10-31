//! Test seams of `runtime/model_loop.rs`.

use crate::runtime::helpers::safe_error;
use crate::runtime::*;

impl crate::runtime::Engine {
    pub(crate) fn has_active_run_for_test(&self, run_id: RunId) -> bool {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&run_id)
    }

    pub(crate) async fn retry_run_setup_terminalization_for_test(
        &self,
        run_id: RunId,
    ) -> Result<(), EngineError> {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run_id)
            .cloned()
            .ok_or(EngineError::MissingRun(run_id))?;
        self.append(
            active.session,
            Some(run_id),
            event_origin("engine:model-loop"),
            Event::run_finished(cookie_agent_protocol::RunOutcome::Failed {
                error: safe_error("run setup terminalization retried"),
                model_error: None,
                resolved_model: None,
            }),
        )
        .await?;
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run_id);
        Ok(())
    }
}
