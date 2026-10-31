//! Test seams of `runtime/messaging/producers.rs`.

use crate::runtime::messaging::producers::ProducerAuthority;
use crate::runtime::*;
use cookie_agent_protocol::*;

impl crate::runtime::Engine {
    pub(crate) fn install_producer_wake_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .producer_wake_hook
            .lock()
            .expect("producer wake hook lock poisoned") =
            Some(Arc::new(crate::runtime::PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(in crate::runtime) async fn wait_for_producer_wake_hook(&self) {
        let hook = self
            .inner
            .test_hooks
            .producer_wake_hook
            .lock()
            .expect("producer wake hook lock poisoned")
            .clone();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("producer wake hook reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn accept_agent_message_direct_for_test(
        &self,
        session: SessionId,
        authority: &ProducerAuthority,
        producer_id: ProducerId,
        mode: ProducerDeliveryMode,
        key: ProducerIdempotencyKey,
        description: SafeDisplayText,
        sender: SessionId,
        sender_agent_type: &str,
        body: String,
        hop: u32,
    ) -> Result<ProducerMessageId, EngineError> {
        self.accept_agent_message_direct(
            session,
            authority,
            producer_id,
            mode,
            key,
            description,
            sender,
            sender_agent_type,
            body,
            crate::runtime::messaging::api::ReplyHint::SendMessage,
            hop,
        )
    }
}
