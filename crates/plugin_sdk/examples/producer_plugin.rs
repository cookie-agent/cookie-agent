use cookie_agent_plugin_sdk::{
    PluginError, PluginServer, ProducerIdempotencyKey, ProducerMessageId, RecoveryResult, SessionId,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), PluginError> {
    PluginServer::builder("producer", env!("CARGO_PKG_VERSION"))
        .enable_producers()
        .on_recovery(restore_external_work)
        .run_stdio()
        .await
}

async fn restore_external_work(context: cookie_agent_plugin_sdk::PluginContext) -> RecoveryResult {
    // A real plugin discovers these sessions and keys from its own durable service.
    let Ok(value) = std::env::var("COOKIE_SESSION_ID") else {
        return Ok(());
    };
    let session = value
        .parse::<SessionId>()
        .map_err(|error| PluginError::Protocol(format!("invalid COOKIE_SESSION_ID: {error}")))?;
    if let Ok(value) = std::env::var("COOKIE_DISCARD_MESSAGE_ID") {
        let message = value.parse::<ProducerMessageId>().map_err(|error| {
            PluginError::Protocol(format!("invalid COOKIE_DISCARD_MESSAGE_ID: {error}"))
        })?;
        // A saved receipt remains addressable without restoring its old registration.
        context.discard_producer_message(session, message).await?;
        return Ok(());
    }
    let key = ProducerIdempotencyKey::new("restored-work").expect("static key is valid");
    let producer = context.register_producer(session).await?;
    producer
        .steer("Restored external work completed", key)
        .await?;
    producer.unregister().await?;
    Ok(())
}
