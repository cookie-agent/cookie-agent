//! How an assistant reply's own lines read in every client: the footer that
//! closes it and the status its thinking blocks' headers give.

use std::time::Duration;

use crate::{
    SessionState,
    usage::{format_cost_usd, format_token_count},
};

/// The text of the footer that closes the assistant block `item_id`:
/// `⚡ 42.1 tps · 12.5K ctx · $0.0040`, with a trailing `· interrupted` when
/// its run was interrupted. The rate is committed output tokens over
/// generation wall time measured between durable event timestamps, so a
/// replayed log yields the identical text; the ctx is the total context the
/// turn left behind (`input_tokens + output_tokens`); the cost shows only
/// while every turn of the block was priced. `None` unless the block has a
/// known end-of-turn context total and at least one timed turn with output
/// over a positive span.
pub fn assistant_footer(state: &SessionState, item_id: u64) -> Option<String> {
    let metrics = state.assistant_metrics.get(&item_id)?;
    let context_tokens = metrics.context_tokens?;
    if metrics.timed_output_tokens == 0 || metrics.generation.is_zero() {
        return None;
    }
    let tps = metrics.timed_output_tokens as f64 / metrics.generation.as_secs_f64();
    let mut footer = format!(
        "⚡ {tps:.1} tps · {} ctx",
        format_token_count(context_tokens)
    );
    if let Some(cost) = metrics.estimated_cost_pico_usd {
        footer.push_str(" · ");
        footer.push_str(&format_cost_usd(cost as f64 / 1_000_000_000_000.0));
    }
    if state.interrupted_assistant_items.contains(&item_id) {
        footer.push_str(" · interrupted");
    }
    Some(footer)
}

/// What a sealed thinking block's header says: `thought for 1m 5s` with the
/// elapsed time the projection recorded ([`SessionState::thinking_duration`])
/// when it ran a second or more, else `thought`.
pub fn thought_label(duration: Option<Duration>) -> String {
    match duration.filter(|duration| duration.as_secs() >= 1) {
        Some(duration) => format!("thought for {}", format_elapsed(duration)),
        None => "thought".to_owned(),
    }
}

/// An elapsed time as compact text: `42s` under a minute, `1m 23s` under
/// an hour, then `1h 4m`.
#[must_use]
pub fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{assistant_footer, thought_label};
    use crate::{AssistantTurnMetrics, SessionState};

    fn state_with(metrics: AssistantTurnMetrics) -> SessionState {
        let mut state = SessionState::default();
        state.assistant_metrics.insert(7, metrics);
        state
    }

    fn timed(output: u64, millis: u64, context: Option<u64>) -> AssistantTurnMetrics {
        AssistantTurnMetrics {
            timed_output_tokens: output,
            generation: Duration::from_millis(millis),
            timed_turns: 1,
            context_tokens: context,
            ..AssistantTurnMetrics::default()
        }
    }

    #[test]
    fn the_footer_reads_rate_context_and_cost() {
        let mut metrics = timed(421, 10_000, Some(12_500));
        metrics.estimated_cost_pico_usd = Some(4_000_000_000);
        assert_eq!(
            assistant_footer(&state_with(metrics), 7).as_deref(),
            Some("⚡ 42.1 tps · 12.5K ctx · $0.0040")
        );
        // A cost of a cent or more shows to the cent; a context under a
        // thousand tokens shows whole.
        metrics.estimated_cost_pico_usd = Some(12_340_000_000);
        metrics.context_tokens = Some(999);
        assert_eq!(
            assistant_footer(&state_with(metrics), 7).as_deref(),
            Some("⚡ 42.1 tps · 999 ctx · $0.01")
        );
    }

    #[test]
    fn an_unpriced_block_has_no_cost_and_an_interrupted_one_says_so() {
        let mut state = state_with(timed(50, 2_000, Some(1_000)));
        assert_eq!(
            assistant_footer(&state, 7).as_deref(),
            Some("⚡ 25.0 tps · 1.0K ctx")
        );
        state.interrupted_assistant_items.insert(7);
        assert_eq!(
            assistant_footer(&state, 7).as_deref(),
            Some("⚡ 25.0 tps · 1.0K ctx · interrupted")
        );
    }

    #[test]
    fn no_footer_without_context_output_or_generation_time() {
        assert_eq!(assistant_footer(&SessionState::default(), 7), None);
        assert_eq!(
            assistant_footer(&state_with(timed(50, 2_000, None)), 7),
            None
        );
        assert_eq!(
            assistant_footer(&state_with(timed(50, 0, Some(1_000))), 7),
            None
        );
        assert_eq!(
            assistant_footer(&state_with(timed(0, 2_000, Some(1_000))), 7),
            None
        );
        // Another block's metrics are not this one's.
        assert_eq!(
            assistant_footer(&state_with(timed(50, 2_000, Some(1_000))), 8),
            None
        );
    }

    #[test]
    fn a_thought_shows_its_time_from_a_second() {
        assert_eq!(thought_label(None), "thought");
        assert_eq!(thought_label(Some(Duration::from_millis(999))), "thought");
        assert_eq!(
            thought_label(Some(Duration::from_secs(1))),
            "thought for 1s"
        );
        assert_eq!(
            thought_label(Some(Duration::from_secs(59))),
            "thought for 59s"
        );
        assert_eq!(
            thought_label(Some(Duration::from_secs(65))),
            "thought for 1m 5s"
        );
    }
}
