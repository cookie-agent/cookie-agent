//! Compaction shortly before a session's prompt cache expires
//! (`docs/site/specs/cache-aware-idle.md` section 4): the TTL each request
//! cached for, stamped on its usage event (4.1); the rule that plans a
//! compaction (4.2); one scheduler task that runs them (4.3); and the
//! compaction itself, which is not activity (4.4, 10).

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cookie_agent_config::{CacheExpiryConfig, CacheExpiryMode};
use cookie_agent_models::adapters::{
    AnthropicCacheTtlConfig, BedrockCacheTtl, CacheStrategyConfig, OpenAiPromptCacheRetention,
    OpenAiPromptCacheTtl,
};
use cookie_agent_protocol::{
    CacheTtlSource, CacheTtlStamp, CheckpointReason, CronJobId, EventPayload as Event, Open,
    PlannedCacheCompaction, SessionId,
};
use jiff::{SignedDuration, Timestamp};
use tokio::{sync::Notify, task::JoinHandle};

use super::{Engine, EngineError, event_origin, helpers::root_id};

/// The origin of what a compaction before expiry records.
pub(crate) const CACHE_EXPIRY_ORIGIN: &str = cookie_agent_engine_store::session::QUIET_ORIGIN;

/// How often the scheduler looks at every candidate again, whatever woke
/// it: configuration reloads and anything a change notice missed land
/// within it.
const FULL_PASS_EVERY: Duration = Duration::from_secs(60);

/// How long the TTL of the `chatgpt` provider is assumed to be: the
/// documented GPT-5.6+ default (section 11.2).
const CHATGPT_ASSUMED_TTL: Duration = Duration::from_secs(30 * 60);

/// The TTL a request cached its conversation for: what the cache strategy
/// it was sent with sets, else what the configuration declares or assumes
/// (section 4.1). `None` when it cached nothing past its system prompt and
/// tools: an Anthropic or Bedrock request without a rolling marker.
pub(crate) fn request_cache_ttl(
    strategy: Option<&CacheStrategyConfig>,
    chatgpt: bool,
    declared: Option<Duration>,
    fallback: Duration,
) -> Option<CacheTtlStamp> {
    let set = |seconds: u64| {
        Some(CacheTtlStamp {
            ttl_secs: seconds,
            source: CacheTtlSource::Set,
        })
    };
    let declared_or = |assumed: Duration| match declared {
        Some(ttl) => Some(CacheTtlStamp {
            ttl_secs: ttl.as_secs(),
            source: CacheTtlSource::Declared,
        }),
        None => Some(CacheTtlStamp {
            ttl_secs: assumed.as_secs().max(1),
            source: CacheTtlSource::Assumed,
        }),
    };
    if chatgpt {
        return declared_or(CHATGPT_ASSUMED_TTL);
    }
    match strategy {
        Some(CacheStrategyConfig::Anthropic(strategy)) => match strategy.rolling? {
            AnthropicCacheTtlConfig::FiveMinutes => set(5 * 60),
            AnthropicCacheTtlConfig::OneHour => set(60 * 60),
        },
        Some(CacheStrategyConfig::Bedrock(strategy)) => {
            match strategy.messages.last()?.cache_point.ttl {
                Some(BedrockCacheTtl::OneHour) => set(60 * 60),
                Some(BedrockCacheTtl::FiveMinutes) | None => set(5 * 60),
            }
        }
        Some(CacheStrategyConfig::OpenAi(strategy)) if strategy.mode.is_some() => {
            match strategy.ttl.unwrap_or(OpenAiPromptCacheTtl::ThirtyMinutes) {
                OpenAiPromptCacheTtl::ThirtyMinutes => set(30 * 60),
                OpenAiPromptCacheTtl::TwelveHours => set(12 * 60 * 60),
            }
        }
        Some(CacheStrategyConfig::OpenAi(strategy))
            if strategy.prompt_cache_retention
                == Some(OpenAiPromptCacheRetention::TwentyFourHours) =>
        {
            set(24 * 60 * 60)
        }
        _ => declared_or(fallback),
    }
}

pub(crate) use crate::events::state::LatestRequest;

/// A compaction the rule plans.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub(crate) due_at: Timestamp,
    pub(crate) expires_at: Timestamp,
    pub(crate) next_fire: Option<(Timestamp, CronJobId)>,
    pub(crate) usage_seq: u64,
}

/// What else the rule reads of a session.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionState {
    pub(crate) held: bool,
    /// A session of its tree has pending work.
    pub(crate) busy: bool,
    /// The usage event its last compaction before expiry followed (rule 6).
    pub(crate) tried: Option<u64>,
    /// Its scheduled jobs' earliest next fire.
    pub(crate) next_fire: Option<(Timestamp, CronJobId)>,
    /// What caching its context again would cost, for a priced model.
    pub(crate) recache_cost_pico_usd: Option<u64>,
}

/// The rule of section 4.2: when, if at all, to compact a session whose
/// latest request is `request`, at `now`.
pub(crate) fn plan(
    config: &CacheExpiryConfig,
    request: &LatestRequest,
    state: &SessionState,
    now: Timestamp,
) -> Option<Plan> {
    if config.mode == CacheExpiryMode::Off || state.held || state.busy {
        return None;
    }
    if state.tried == Some(request.usage_seq) {
        return None;
    }
    let ttl = Duration::from_secs(request.stamp.ttl_secs);
    let expires_at = request.stamp.expires_at(request.sent_at)?;
    if now >= expires_at {
        return None;
    }
    match (&state.next_fire, config.mode) {
        // A fire before expiry reads a warm cache.
        (Some((fire, _)), _) if *fire <= expires_at => return None,
        (None, CacheExpiryMode::Scheduled) => return None,
        _ => {}
    }
    if !worth_compacting(config, request.tokens, state.recache_cost_pico_usd) {
        return None;
    }
    let slack = SignedDuration::try_from(config.effective_slack(ttl)).ok()?;
    let due_at = expires_at.checked_sub(slack).ok()?;
    Some(Plan {
        due_at,
        expires_at,
        next_fire: state.next_fire.clone(),
        usage_seq: request.usage_seq,
    })
}

/// Condition 4: a floor at `0` or less is off; with both off everything
/// passes, and otherwise reaching either one is enough. The cost floor
/// needs a priced model.
pub(crate) fn worth_compacting(
    config: &CacheExpiryConfig,
    tokens: u64,
    recache_cost_pico_usd: Option<u64>,
) -> bool {
    let token_floor = u64::try_from(config.min_tokens)
        .ok()
        .filter(|floor| *floor > 0);
    let cost_floor = (config.min_cost_usd > 0.0).then_some(config.min_cost_usd);
    if token_floor.is_none() && cost_floor.is_none() {
        return true;
    }
    #[allow(clippy::cast_precision_loss)]
    let cost_usd = recache_cost_pico_usd.map(|pico| pico as f64 / 1e12);
    token_floor.is_some_and(|floor| tokens >= floor)
        || cost_floor.is_some_and(|floor| cost_usd.is_some_and(|cost| cost >= floor))
}

/// The scheduler's state, owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct CacheExpiryState {
    /// The plan of each session that has one.
    plans: Mutex<HashMap<SessionId, Plan>>,
    /// Each session's latest request as last read, for sessions read while
    /// resident; a paged-out session's stays as it was.
    requests: Mutex<HashMap<SessionId, Option<(LatestRequest, bool)>>>,
    /// The usage event each session's last compaction before expiry
    /// followed (rule 6).
    tried: Mutex<HashMap<SessionId, u64>>,
    /// Sessions to read again.
    dirty: Mutex<HashSet<SessionId>>,
    /// Every candidate is to be looked at again.
    full_pass: AtomicBool,
    /// Sessions compacting now.
    running: Mutex<HashSet<SessionId>>,
    wake: Arc<Notify>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Engine {
    /// The cache TTL to stamp on the usage of a request of `binding` sent
    /// with `strategy`.
    pub(in crate::runtime) fn stamp_cache_ttl(
        &self,
        strategy: Option<&CacheStrategyConfig>,
        binding: &cookie_agent_protocol::FrozenModelBinding,
    ) -> Option<CacheTtlStamp> {
        let config = self.inner.config.load();
        let model = &binding.selection.model;
        let chatgpt = matches!(
            config.runtime.providers.get(&model.provider_id()),
            Some(cookie_agent_models::ProviderDefinition::ChatGpt(_))
        );
        let declared = config.runtime.model_cache_ttls.get(model).copied();
        request_cache_ttl(
            strategy,
            chatgpt,
            declared,
            config.runtime.context_compaction.cache_expiry.fallback_ttl,
        )
    }

    /// The cache TTL to stamp on the usage of ACP agent `agent`'s turn on
    /// `runtime`: its declaration's `cache_ttl`, else none.
    pub(in crate::runtime) fn stamp_acp_cache_ttl(
        &self,
        runtime: &cookie_agent_protocol::RuntimeId,
        agent: &cookie_agent_protocol::AgentId,
    ) -> Option<CacheTtlStamp> {
        let agent = cookie_agent_identity::AgentId::new(agent.as_str()).ok()?;
        let agent = &agent;
        let config = self.inner.config.load();
        let declaration = config
            .runtimes
            .iter()
            .find(|(id, _)| id.as_str() == runtime.as_str())
            .and_then(|(_, template)| template.acp_agents.get(agent))
            .or_else(|| config.runtime.acp.agents.get(agent))?;
        declaration.cache_ttl_duration().map(|ttl| CacheTtlStamp {
            ttl_secs: ttl.as_secs(),
            source: CacheTtlSource::Declared,
        })
    }

    /// Asks the scheduler to read `session` again.
    pub(in crate::runtime) fn cache_expiry_touch(&self, session: SessionId) {
        lock(&self.inner.cache_expiry.dirty).insert(session);
        self.inner.cache_expiry.wake.notify_one();
    }

    /// Starts the scheduler (section 4.3), like the cron scheduler, and the
    /// task that turns session index changes into reads.
    pub(in crate::runtime) fn start_cache_expiry_scheduler(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let mut changes = self.inner.store.subscribe_index_changes();
        let forwarder_weak = weak.clone();
        let forwarder = runtime.spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                let change = changes.recv().await;
                let Some(inner) = forwarder_weak.upgrade() else {
                    break;
                };
                let state = &inner.cache_expiry;
                match change {
                    Ok(cookie_agent_protocol::SessionsChangedNotification::Upserted {
                        session,
                    }) => {
                        lock(&state.dirty).insert(session.session_id);
                        state.wake.notify_one();
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {
                        state.full_pass.store(true, Ordering::Release);
                        lock(&state.requests).clear();
                        state.wake.notify_one();
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        let mut crons = self.inner.crons.subscribe();
        let cron_weak = weak.clone();
        let cron_watch = runtime.spawn(async move {
            while crons.changed().await.is_ok() {
                let Some(inner) = cron_weak.upgrade() else {
                    break;
                };
                inner.cache_expiry.full_pass.store(true, Ordering::Release);
                inner.cache_expiry.wake.notify_one();
            }
        });
        let task = runtime.spawn(async move {
            let mut last_full_pass: Option<tokio::time::Instant> = None;
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let engine = Engine { inner };
                if engine.admission_tasks_closing() || engine.restart_due() {
                    break;
                }
                let full = engine
                    .inner
                    .cache_expiry
                    .full_pass
                    .swap(false, Ordering::AcqRel)
                    || last_full_pass.is_none_or(|at| at.elapsed() >= FULL_PASS_EVERY);
                if full {
                    last_full_pass = Some(tokio::time::Instant::now());
                }
                engine.cache_expiry_pass(full).await;
                let now = Timestamp::now();
                let next_due = lock(&engine.inner.cache_expiry.plans)
                    .values()
                    .map(|plan| plan.due_at)
                    .min();
                let until_due = next_due.map(|due| {
                    let millis = due.as_millisecond().saturating_sub(now.as_millisecond());
                    Duration::from_millis(u64::try_from(millis).unwrap_or(0))
                });
                let sleep = until_due.unwrap_or(FULL_PASS_EVERY).min(FULL_PASS_EVERY);
                let wake = Arc::clone(&engine.inner.cache_expiry.wake);
                drop(engine);
                tokio::select! {
                    () = tokio::time::sleep(sleep) => {}
                    () = wake.notified() => {}
                }
            }
        });
        lock(&self.inner.cache_expiry.tasks).extend([forwarder, cron_watch, task]);
    }

    /// Takes the scheduler's tasks, for shutdown to stop them.
    pub(in crate::runtime) fn take_cache_expiry_tasks(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *lock(&self.inner.cache_expiry.tasks))
    }

    /// One pass: reads the sessions to read, plans every candidate, tells
    /// clients what changed, and starts the compactions that are due.
    async fn cache_expiry_pass(&self, full: bool) {
        let config = self
            .inner
            .config
            .load()
            .runtime
            .context_compaction
            .cache_expiry
            .clone();
        let state = &self.inner.cache_expiry;
        let dirty = std::mem::take(&mut *lock(&state.dirty));
        {
            // A paged-out session's history cannot have changed: its last
            // reading stands. Reading it again would load it back into
            // memory, as the notice of its own plan would otherwise do.
            let mut requests = lock(&state.requests);
            for session in &dirty {
                if self.inner.store.is_resident(*session) {
                    requests.remove(session);
                }
            }
        }
        let mut candidates: HashSet<SessionId> = lock(&state.plans).keys().copied().collect();
        if config.mode != CacheExpiryMode::Off {
            candidates.extend(self.inner.crons.waiting_targets());
            if config.mode == CacheExpiryMode::Always {
                candidates.extend(
                    self.inner
                        .store
                        .all()
                        .iter()
                        .map(|session| session.meta.session_id),
                );
            }
        }
        if !full {
            candidates.retain(|session| {
                dirty.contains(session) || lock(&state.plans).contains_key(session)
            });
        }
        let busy = self.busy_roots();
        let now = Timestamp::now();
        let mut due = Vec::new();
        for session in candidates {
            let planned = if config.mode == CacheExpiryMode::Off {
                None
            } else {
                self.plan_session(session, &config, &busy, now).await
            };
            if let Some(plan) = &planned
                && plan.due_at <= now
                && !lock(&state.running).contains(&session)
            {
                due.push((session, plan.clone()));
            }
            let changed = {
                let mut plans = lock(&state.plans);
                let previous = match planned.clone() {
                    Some(plan) => plans.insert(session, plan),
                    None => plans.remove(&session),
                };
                previous != planned
            };
            if changed {
                self.announce_cache_compaction(session, planned.as_ref(), false);
            }
        }
        for (session, plan) in due {
            lock(&state.running).insert(session);
            let engine = self.clone();
            tokio::spawn(async move {
                engine.compact_before_expiry(session, plan).await;
                lock(&engine.inner.cache_expiry.running).remove(&session);
                engine.cache_expiry_touch(session);
            });
        }
    }

    /// Tells clients `session`'s plan, `running` or not.
    fn announce_cache_compaction(&self, session: SessionId, plan: Option<&Plan>, running: bool) {
        let planned = plan.map(|plan| PlannedCacheCompaction {
            at: plan.due_at,
            expires_at: plan.expires_at,
            next_fire: plan.next_fire.as_ref().map(|(fire, _)| *fire),
            job_id: plan.next_fire.as_ref().map(|(_, job)| job.clone()),
            running,
        });
        if self.inner.store.record_cache_compaction(session, planned) {
            self.inner.store.announce_live_waiting(session);
        }
    }

    /// `session`'s plan now. A session paged out is read from its last
    /// reading; one never read is loaded once.
    async fn plan_session(
        &self,
        session: SessionId,
        config: &CacheExpiryConfig,
        busy: &HashSet<SessionId>,
        now: Timestamp,
    ) -> Option<Plan> {
        let state = &self.inner.cache_expiry;
        let cached = lock(&state.requests).get(&session).cloned();
        let reading = match (self.inner.store.get_resident(session), cached) {
            (Some(projection), _) => {
                let reading = projection
                    .log
                    .state()
                    .latest_request()
                    .cloned()
                    .map(|request| (request, projection.meta.held.is_some()));
                lock(&state.requests).insert(session, reading.clone());
                reading
            }
            (None, Some(cached)) => cached,
            (None, None) => {
                let store = Arc::clone(&self.inner.store);
                let projection = super::blocking_io::run(move || store.get(session))
                    .await
                    .ok()?
                    .ok()?;
                let reading = projection
                    .log
                    .state()
                    .latest_request()
                    .cloned()
                    .map(|request| (request, projection.meta.held.is_some()));
                lock(&state.requests).insert(session, reading.clone());
                reading
            }
        };
        let (request, held) = reading?;
        let origin = self.inner.store.origin(session).ok()?;
        let root = root_id(&origin, session);
        let config_now = self.inner.config.load();
        let recache_cost_pico_usd = crate::usage::recache_cost_pico_usd(
            &request.model,
            request.tokens,
            &config_now.runtime.pricing,
            &*self.inner.model_manager.current(),
        );
        let session_state = SessionState {
            held,
            busy: busy.contains(&root),
            tried: lock(&state.tried).get(&session).copied(),
            next_fire: self.inner.crons.next_fire_into(session),
            recache_cost_pico_usd,
        };
        plan(config, &request, &session_state, now)
    }

    /// Runs `plan` for `session` (section 4.4): checks the rule again on
    /// the session as it is now, then compacts it, quietly.
    async fn compact_before_expiry(&self, session: SessionId, plan: Plan) {
        let config = self
            .inner
            .config
            .load()
            .runtime
            .context_compaction
            .cache_expiry
            .clone();
        lock(&self.inner.cache_expiry.requests).remove(&session);
        let current = {
            let _residency = self
                .lock_residency("checking a compaction before cache expiry of", session)
                .await;
            let busy = self.busy_roots();
            self.plan_session(session, &config, &busy, Timestamp::now())
                .await
        };
        let Some(current) = current.filter(|current| {
            current.usage_seq == plan.usage_seq && current.due_at <= Timestamp::now()
        }) else {
            return;
        };
        // Rule 7: a timer that woke after expiry compacts nothing.
        if Timestamp::now() >= current.expires_at {
            return;
        }
        lock(&self.inner.cache_expiry.tried).insert(session, current.usage_seq);
        self.announce_cache_compaction(session, Some(&current), true);
        let reason = CheckpointReason::CacheExpiry {
            expires_at: current.expires_at,
            next_fire: current.next_fire.as_ref().map(|(fire, _)| *fire),
            job_id: current.next_fire.as_ref().map(|(_, job)| job.clone()),
        };
        self.inner.store.set_quiet(session, true);
        let result = self
            .compact_session_with_reason(
                session,
                event_origin(CACHE_EXPIRY_ORIGIN),
                Some(Open::Known(reason)),
            )
            .await;
        let failure = match result {
            Ok(result) if result.compacted => None,
            Ok(result) => result.cancellation_reason,
            // Someone used the session first: nothing to say.
            Err(EngineError::SessionRunning(_)) => None,
            Err(error) => Some(error.to_string()),
        };
        if let Some(message) = failure {
            let _ = self
                .append(
                    session,
                    None,
                    event_origin(CACHE_EXPIRY_ORIGIN),
                    Event::Diagnostic {
                        source: cookie_agent_protocol::DiagnosticSource::engine("cache-expiry")
                            .expect("static diagnostic source is valid"),
                        kind: cookie_agent_protocol::EngineDiagnosticKind::CacheExpiryCompaction
                            .label(),
                        message: format!(
                            "compaction before the prompt cache expired failed: {message}"
                        ),
                        count: None,
                        detail: None,
                    },
                )
                .await;
        }
        self.inner.store.set_quiet(session, false);
        lock(&self.inner.cache_expiry.plans).remove(&session);
        self.announce_cache_compaction(session, None, false);
    }
}

#[cfg(test)]
mod tests;
