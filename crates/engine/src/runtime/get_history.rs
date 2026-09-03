use cookie_agent_protocol::SessionId;

use super::{Engine, EngineError, Event, compaction::active_compaction_binding};
use crate::model_history;

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
        let events = self.inner.store.get(session)?.log.event_snapshot();
        let run = events
            .iter()
            .rev()
            .find_map(|event| {
                matches!(event.payload, Event::RunStarted { .. })
                    .then_some(event.run_id)
                    .flatten()
            })
            .ok_or(EngineError::NoRunnableModel)?;
        let policy = self.historical_title_policy(&events, run)?;
        let binding = active_compaction_binding(&policy, &events, run)?;
        let composed_prompt = self.run_agent_prompt(session, run)?;
        match view {
            EngineHistoryView::Assembled => Ok(model_history::assemble_model_context(
                &events,
                &self.inner.artifacts,
                binding,
                &composed_prompt,
            )?
            .history),
            EngineHistoryView::Full => Ok(model_history::assemble_full_history(
                &events,
                &self.inner.artifacts,
                binding,
                &composed_prompt,
            )?),
        }
    }
}
