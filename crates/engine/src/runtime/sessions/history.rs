use cookie_agent_protocol::SessionId;

use crate::model_history;
use crate::runtime::{Engine, EngineError, compaction::active_compaction_binding};

/// Selects the history projection returned by [`Engine::get_history`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EngineHistoryView {
    /// History turns assembled after the latest checkpoint, as for a normal model request.
    ///
    /// Native provider context and replay-decision metadata are not history turns and are omitted
    /// from this projection.
    #[default]
    Assembled,
    /// Replayable history turns from the start of the visible event log, without checkpoint
    /// truncation.
    Full,
}

impl Engine {
    /// Returns a live history projection for the given session.
    ///
    /// Callable by anything holding an engine reference. The result reflects state at query time
    /// and may include events appended after the current model request started, such as earlier
    /// tool results from the same in-flight turn. Tool providers needing history should retain an
    /// [`Engine`] handle, following the delegate-provider pattern; the engine is deliberately not
    /// injected into every tool context.
    pub async fn get_history(
        &self,
        session: SessionId,
        view: EngineHistoryView,
    ) -> Result<Vec<oven_sdk::HistoryTurn>, EngineError> {
        let log = self.inner.store.log(session)?;
        let run = log
            .state()
            .latest_run_started()
            .and_then(|started| started.run_id)
            .ok_or(EngineError::NoRunnableModel)?;
        let policy = self.historical_title_policy(&log, run)?;
        let binding = active_compaction_binding(&policy, &log, run)?;
        let composed_prompt = self.run_agent_prompt(session, run)?;
        match view {
            EngineHistoryView::Assembled => Ok(model_history::assemble_model_context(
                &log.model_context_events()?,
                &self.inner.artifacts,
                binding,
                &composed_prompt,
            )?
            .history),
            // Everything the session recorded, which its answer holds too.
            EngineHistoryView::Full => Ok(model_history::assemble_full_history(
                &log.history()?.collect::<Result<Vec<_>, _>>()?,
                &self.inner.artifacts,
                binding,
                &composed_prompt,
            )?),
        }
    }
}
