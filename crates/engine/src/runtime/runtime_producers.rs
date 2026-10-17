//! Runtimes and plugins as producer sources (`docs/site/specs/external-producers.md`
//! section 5): a runtime that declares `producer_messaging` produces as
//! `runtime:<runtime ID>`, over its live connection, and only for the
//! sessions attached to one of its capability instances.

use std::sync::Arc;

use cookie_agent_protocol::{
    ProducerDiscardParams, ProducerRegisterParams, ProducerSendParams, ProducerSource,
    ProducerSourceState, ProducerUnregisterParams, SessionId,
};
use cookie_agent_runtime_api::protocol::method;

use super::{
    Engine, EngineError,
    producers::{ExternalConnection, ExternalProducerOperation, ExternalProducerResult},
};
use crate::runtimes::{InstanceConnection, RuntimeProducerCall, RuntimeProducerHost};

impl Engine {
    /// Opens producer messaging on every runtime connection that declares
    /// it, as instances start and reconnect, and reconciles the sessions
    /// whose producers a connection change affects.
    pub(super) fn install_runtime_producers(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let (changed, mut changes) = tokio::sync::watch::channel(0_u64);
        let weak = Arc::downgrade(&self.inner);
        let host = Arc::new(RuntimeProducerHost {
            call: Box::new(move |call| {
                let weak = weak.clone();
                Box::pin(async move {
                    let inner = weak.upgrade().ok_or("the daemon is stopping")?;
                    Engine { inner }
                        .answer_runtime_producer(call)
                        .await
                        .map_err(|error| error.to_string())
                })
            }),
            changed: Box::new(move || {
                changed.send_modify(|revision| *revision = revision.wrapping_add(1));
            }),
        });
        let weak = Arc::downgrade(&self.inner);
        let mut instances = self.inner.runtimes.subscribe();
        let mut plugins = self.inner.plugins.subscribe_changes();
        self.spawn_admission_task(&runtime, async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let engine = Engine { inner };
                for instance in engine.inner.runtimes.published_instances() {
                    if let Some(connection) = instance.connection() {
                        connection.start_producers(&host);
                    }
                }
                for connection in engine.inner.plugins.connections() {
                    connection.start_producers(&host);
                }
                engine.reconcile_external_producer_sessions().await;
                drop(engine);
                tokio::select! {
                    changed = instances.changed() => if changed.is_err() { return },
                    changed = plugins.changed() => if changed.is_err() { return },
                    changed = changes.changed() => if changed.is_err() { return },
                }
            }
        });
    }

    async fn answer_runtime_producer(
        &self,
        call: RuntimeProducerCall,
    ) -> Result<serde_json::Value, EngineError> {
        let source = call.source;
        let params = call.params;
        let decode = |error: serde_json::Error| EngineError::Producer(error.to_string());
        let operation = match call.method.as_str() {
            method::PRODUCER_REGISTER => ExternalProducerOperation::Register(
                serde_json::from_value::<ProducerRegisterParams>(params).map_err(decode)?,
            ),
            method::PRODUCER_SEND => ExternalProducerOperation::Send(
                serde_json::from_value::<ProducerSendParams>(params).map_err(decode)?,
            ),
            method::PRODUCER_UNREGISTER => ExternalProducerOperation::Unregister(
                serde_json::from_value::<ProducerUnregisterParams>(params).map_err(decode)?,
            ),
            method::PRODUCER_DISCARD => ExternalProducerOperation::Discard(
                serde_json::from_value::<ProducerDiscardParams>(params).map_err(decode)?,
            ),
            other => {
                return Err(EngineError::Producer(format!(
                    "unknown producer method `{other}`"
                )));
            }
        };
        let result = self
            .dispatch_external_producer(
                ExternalConnection {
                    source,
                    epoch: call.epoch,
                },
                operation,
            )
            .await?;
        let value = match result {
            ExternalProducerResult::Register(result) => serde_json::to_value(result),
            ExternalProducerResult::Send(result) => serde_json::to_value(result),
            ExternalProducerResult::Unregister(result) => serde_json::to_value(result),
            ExternalProducerResult::Discard(result) => serde_json::to_value(result),
        };
        value.map_err(|error| EngineError::Producer(error.to_string()))
    }

    /// The live, producing connection `epoch` of the runtime of `source`,
    /// through one of its open instances.
    fn runtime_source_connection(
        &self,
        source: &ProducerSource,
        epoch: u64,
    ) -> Option<InstanceConnection> {
        let runtime_id = source.runtime_id()?;
        self.inner
            .runtimes
            .published_instances()
            .into_iter()
            .find_map(|instance| {
                let connection = instance.connection()?;
                (connection.epoch() == epoch
                    && !connection.is_closed()
                    && connection.produces()
                    && instance.info().id() == runtime_id)
                    .then_some(connection)
            })
    }

    /// Whether `epoch` is the live connection of the runtime of `source`.
    pub(super) fn runtime_source_is_current(&self, source: &ProducerSource, epoch: u64) -> bool {
        self.runtime_source_connection(source, epoch).is_some()
    }

    /// Whether the runtime of `source`, over connection `epoch`, is
    /// attached to `session`.
    pub(super) fn runtime_source_attached(
        &self,
        source: &ProducerSource,
        epoch: u64,
        session: SessionId,
    ) -> bool {
        if self.runtime_source_connection(source, epoch).is_none() {
            return false;
        }
        let Ok(projection) = self.inner.store.get(session) else {
            return false;
        };
        self.attachments_of(&projection).iter().any(|attachment| {
            self.inner
                .runtimes
                .instance(attachment)
                .and_then(|attached| attached.connection())
                .is_some_and(|connection| connection.epoch() == epoch)
        })
    }

    /// The recovery state of every producer-capable runtime attached to
    /// `session`.
    pub(super) fn runtime_producer_source_states(
        &self,
        session: SessionId,
    ) -> Vec<ProducerSourceState> {
        let Ok(projection) = self.inner.store.get(session) else {
            return Vec::new();
        };
        let mut states = Vec::<ProducerSourceState>::new();
        for attachment in self.attachments_of(&projection) {
            let Some(instance) = self.inner.runtimes.instance(&attachment) else {
                continue;
            };
            let Some(status) = instance
                .connection()
                .and_then(|connection| connection.producer_status())
            else {
                continue;
            };
            let Ok(source) = ProducerSource::runtime(instance.info().id()) else {
                continue;
            };
            if !states.iter().any(|state| state.source == source) {
                states.push(ProducerSourceState { source, status });
            }
        }
        states
    }
}
