use std::{sync::Arc, time::Duration};

use cookie_agent_config::{CacheExpiryConfig, CacheExpiryMode};
use cookie_agent_models::adapters::{
    AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig, BedrockCachePoint, BedrockCacheStrategy,
    BedrockCacheTtl, BedrockMessageCachePoint, CacheStrategyConfig, OpenAiCacheMode,
    OpenAiCacheStrategyConfig, OpenAiPromptCacheRetention, OpenAiPromptCacheTtl,
};
use cookie_agent_protocol::{
    CacheTtlSource, CacheTtlStamp, ContextCheckpoint, ContextCheckpointBoundaries,
    ContextCheckpointBudgets, ContextCheckpointCommit, CronJobId, EventPayload as Event,
    InternalSummaryCheckpoint, ResolvedModelRef, SessionId, StoredEvent, SummaryByteLimit, Usage,
};
use jiff::Timestamp;

use super::{LatestRequest, SessionState, plan, request_cache_ttl, worth_compacting};

const FALLBACK: Duration = Duration::from_secs(17 * 60);

fn stamp(ttl_secs: u64, source: CacheTtlSource) -> Option<CacheTtlStamp> {
    Some(CacheTtlStamp { ttl_secs, source })
}

fn anthropic(rolling: Option<AnthropicCacheTtlConfig>) -> CacheStrategyConfig {
    CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
        system: Some(AnthropicCacheTtlConfig::OneHour),
        tools: Some(AnthropicCacheTtlConfig::OneHour),
        rolling,
    })
}

fn openai(
    mode: Option<OpenAiCacheMode>,
    ttl: Option<OpenAiPromptCacheTtl>,
    retention: Option<OpenAiPromptCacheRetention>,
) -> CacheStrategyConfig {
    CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
        prompt_cache_key: None,
        prompt_cache_retention: retention,
        mode,
        ttl,
        system: false,
        rolling: true,
    })
}

#[test]
fn the_ttl_is_the_rolling_marker_the_request_was_sent_with() {
    use AnthropicCacheTtlConfig::{FiveMinutes, OneHour};
    let ttl =
        |strategy: CacheStrategyConfig| request_cache_ttl(Some(&strategy), false, None, FALLBACK);
    assert_eq!(
        ttl(anthropic(Some(OneHour))),
        stamp(3_600, CacheTtlSource::Set)
    );
    assert_eq!(
        ttl(anthropic(Some(FiveMinutes))),
        stamp(300, CacheTtlSource::Set)
    );
    assert_eq!(ttl(anthropic(None)), None, "no rolling marker, no clock");
    let bedrock = |point: Option<BedrockCacheTtl>| {
        CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
            system: None,
            tools: None,
            messages: vec![BedrockMessageCachePoint {
                history_index: usize::MAX,
                cache_point: BedrockCachePoint { ttl: point },
            }],
        })
    };
    assert_eq!(
        ttl(bedrock(Some(BedrockCacheTtl::OneHour))),
        stamp(3_600, CacheTtlSource::Set)
    );
    assert_eq!(ttl(bedrock(None)), stamp(300, CacheTtlSource::Set));
    assert_eq!(
        ttl(CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
            system: None,
            tools: None,
            messages: Vec::new(),
        })),
        None
    );
}

#[test]
fn openai_controls_and_retention_set_the_ttl_and_everything_else_falls_back() {
    let ttl = |strategy: CacheStrategyConfig, declared: Option<Duration>| {
        request_cache_ttl(Some(&strategy), false, declared, FALLBACK)
    };
    let implicit = Some(OpenAiCacheMode::Implicit);
    assert_eq!(
        ttl(openai(implicit, None, None), None),
        stamp(1_800, CacheTtlSource::Set)
    );
    assert_eq!(
        ttl(
            openai(implicit, Some(OpenAiPromptCacheTtl::TwelveHours), None),
            None
        ),
        stamp(43_200, CacheTtlSource::Set)
    );
    assert_eq!(
        ttl(
            openai(
                None,
                None,
                Some(OpenAiPromptCacheRetention::TwentyFourHours)
            ),
            None
        ),
        stamp(86_400, CacheTtlSource::Set)
    );
    // A declared TTL never replaces one cookie sets.
    assert_eq!(
        ttl(openai(implicit, None, None), Some(Duration::from_secs(60))),
        stamp(1_800, CacheTtlSource::Set)
    );
    assert_eq!(
        ttl(
            openai(None, None, Some(OpenAiPromptCacheRetention::InMemory)),
            None
        ),
        stamp(FALLBACK.as_secs(), CacheTtlSource::Assumed)
    );
    assert_eq!(
        request_cache_ttl(None, false, Some(Duration::from_secs(7_200)), FALLBACK),
        stamp(7_200, CacheTtlSource::Declared)
    );
    assert_eq!(
        request_cache_ttl(None, false, Some(Duration::ZERO), FALLBACK),
        stamp(0, CacheTtlSource::Declared),
        "0s never expires"
    );
}

#[test]
fn chatgpt_sign_ins_assume_thirty_minutes_unless_declared() {
    let strategy = openai(
        Some(OpenAiCacheMode::Implicit),
        Some(OpenAiPromptCacheTtl::TwelveHours),
        None,
    );
    assert_eq!(
        request_cache_ttl(Some(&strategy), true, None, FALLBACK),
        stamp(1_800, CacheTtlSource::Assumed)
    );
    assert_eq!(
        request_cache_ttl(
            Some(&strategy),
            true,
            Some(Duration::from_secs(600)),
            FALLBACK
        ),
        stamp(600, CacheTtlSource::Declared)
    );
}

fn config(mode: CacheExpiryMode, min_tokens: i64, min_cost_usd: f64) -> CacheExpiryConfig {
    CacheExpiryConfig {
        mode,
        slack: Duration::from_secs(4 * 60),
        fallback_ttl: FALLBACK,
        min_tokens,
        min_cost_usd,
    }
}

#[test]
fn floors_are_off_at_zero_or_less_and_either_one_is_enough() {
    let pico = |usd: f64| Some((usd * 1e12) as u64);
    // Both off: everything passes.
    assert!(worth_compacting(
        &config(CacheExpiryMode::Always, 0, 0.0),
        10,
        None
    ));
    assert!(worth_compacting(
        &config(CacheExpiryMode::Always, -5, -1.0),
        10,
        None
    ));
    // Tokens only.
    let tokens = config(CacheExpiryMode::Always, 1_000, 0.0);
    assert!(worth_compacting(&tokens, 1_000, None));
    assert!(!worth_compacting(&tokens, 999, pico(100.0)));
    // Cost only: priced models, above the floor.
    let cost = config(CacheExpiryMode::Always, 0, 0.5);
    assert!(worth_compacting(&cost, 1, pico(0.5)));
    assert!(!worth_compacting(&cost, 1, pico(0.49)));
    assert!(
        !worth_compacting(&cost, 10_000_000, None),
        "unpriced never reaches it"
    );
    // Both: either.
    let both = config(CacheExpiryMode::Always, 1_000, 0.5);
    assert!(worth_compacting(&both, 1_000, None));
    assert!(worth_compacting(&both, 10, pico(0.6)));
    assert!(!worth_compacting(&both, 10, pico(0.1)));
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

fn model() -> ResolvedModelRef {
    crate::model_history::wire_model(&crate::test_support::model_binding_named("fallback-zero"))
}

fn request(ttl_secs: u64) -> LatestRequest {
    LatestRequest {
        usage_seq: 42,
        sent_at: at(0),
        stamp: CacheTtlStamp {
            ttl_secs,
            source: CacheTtlSource::Set,
        },
        tokens: 50_000,
        model: model(),
    }
}

fn job(after: i64) -> Option<(Timestamp, CronJobId)> {
    Some((at(after), CronJobId::new("k3v9q2xa").unwrap()))
}

#[test]
fn a_session_whose_job_fires_after_expiry_compacts_slack_before_it() {
    let scheduled = config(CacheExpiryMode::Scheduled, 1_000, 0.0);
    let state = SessionState {
        next_fire: job(7_200),
        ..SessionState::default()
    };
    let planned = plan(&scheduled, &request(3_600), &state, at(60)).expect("planned");
    assert_eq!(planned.expires_at, at(3_600));
    assert_eq!(planned.due_at, at(3_600 - 240), "slack under ttl / 5");
    assert_eq!(planned.usage_seq, 42);
    // A short TTL caps the slack at a fifth of it.
    let short = plan(&scheduled, &request(600), &state, at(0)).expect("planned");
    assert_eq!(short.due_at, at(600 - 120));
}

#[test]
fn the_rule_skips_what_section_4_2_skips() {
    let scheduled = config(CacheExpiryMode::Scheduled, 1_000, 0.0);
    let with_job = SessionState {
        next_fire: job(7_200),
        ..SessionState::default()
    };
    let ttl = request(3_600);
    // A fire before expiry reads a warm cache.
    let early = SessionState {
        next_fire: job(1_000),
        ..SessionState::default()
    };
    assert_eq!(plan(&scheduled, &ttl, &early, at(0)), None);
    // `scheduled` needs a job; `always` does not.
    assert_eq!(
        plan(&scheduled, &ttl, &SessionState::default(), at(0)),
        None
    );
    let always = config(CacheExpiryMode::Always, 1_000, 0.0);
    assert!(plan(&always, &ttl, &SessionState::default(), at(0)).is_some());
    // Off, held, busy, already tried, too small, expired, never expiring.
    let off = config(CacheExpiryMode::Off, 1_000, 0.0);
    assert_eq!(plan(&off, &ttl, &with_job, at(0)), None);
    for state in [
        SessionState {
            held: true,
            ..with_job.clone()
        },
        SessionState {
            busy: true,
            ..with_job.clone()
        },
        SessionState {
            tried: Some(42),
            ..with_job.clone()
        },
    ] {
        assert_eq!(plan(&scheduled, &ttl, &state, at(0)), None);
    }
    assert!(
        plan(
            &scheduled,
            &ttl,
            &SessionState {
                tried: Some(41),
                ..with_job.clone()
            },
            at(0)
        )
        .is_some()
    );
    let small = config(CacheExpiryMode::Scheduled, 60_000, 0.0);
    assert_eq!(plan(&small, &ttl, &with_job, at(0)), None);
    assert_eq!(plan(&scheduled, &ttl, &with_job, at(3_600)), None, "lapsed");
    assert_eq!(
        plan(&scheduled, &request(0), &with_job, at(0)),
        None,
        "never expires"
    );
}

fn event(seq: u64, second: i64, payload: Event) -> Arc<StoredEvent> {
    Arc::new(StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: None,
        seq,
        timestamp: at(second),
        payload,
    })
}

fn usage(seq: u64, second: i64, cache_ttl: Option<CacheTtlStamp>) -> Arc<StoredEvent> {
    event(
        seq,
        second,
        Event::ModelUsageRecorded {
            model_turn_seq: seq,
            agent_id: cookie_agent_protocol::AgentId::new("primary").unwrap(),
            resolved_model: model(),
            usage: Usage {
                input_tokens: Some(30_000),
                output_tokens: Some(2_000),
                ..Usage::default()
            },
            estimated_cost_pico_usd: None,
            cache_ttl,
        },
    )
}

fn checkpoint(seq: u64) -> Arc<StoredEvent> {
    let summary = InternalSummaryCheckpoint::new(
        "summary".into(),
        cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
        cookie_agent_protocol::InternalAgentRunId::new_v7(),
        SummaryByteLimit::new(1_024).unwrap(),
    )
    .unwrap();
    event(
        seq,
        0,
        Event::ContextCheckpointCommitted {
            commit: ContextCheckpointCommit {
                checkpoint: ContextCheckpoint::InternalSummary {
                    checkpoint: summary,
                    native: None,
                    folded_tools: Vec::new(),
                },
                boundaries: ContextCheckpointBoundaries {
                    source_from_seq: 1,
                    source_through_seq: seq - 1,
                    input_through_seq: seq - 1,
                    prior_checkpoint_seq: None,
                    recent_from_seq: None,
                },
                budgets: ContextCheckpointBudgets {
                    context_limit_tokens: 100,
                    trigger_tokens: 70,
                    input_tokens_before: 50,
                    input_tokens_after: 10,
                    max_summary_bytes: SummaryByteLimit::new(1_024).unwrap(),
                    keep_recent_tokens: 0,
                },
                reason: None,
            },
        },
    )
}

/// What the log's fold holds of `events`.
fn latest_request(events: &[Arc<StoredEvent>]) -> Option<LatestRequest> {
    let mut state = crate::events::LogState::default();
    for event in events {
        state.observe(event);
    }
    state.latest_request().cloned()
}

#[test]
fn the_latest_stamped_request_is_read_back_unless_a_checkpoint_follows_it() {
    let hour = stamp(3_600, CacheTtlSource::Set);
    let events = vec![usage(1, 10, hour), usage(2, 20, hour)];
    let latest = latest_request(&events).expect("a request");
    assert_eq!(
        (latest.usage_seq, latest.sent_at, latest.tokens),
        (2, at(20), 32_000)
    );
    let compacted = vec![usage(1, 10, hour), checkpoint(2)];
    assert_eq!(latest_request(&compacted), None);
    let unstamped = vec![usage(1, 10, hour), usage(2, 20, None)];
    assert_eq!(
        latest_request(&unstamped),
        None,
        "the latest request has no clock"
    );
}
