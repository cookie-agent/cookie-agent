use cookie_agent_engine::events::OutputMessage;
use cookie_agent_protocol::{
    EventPayload, EventSubscriptionMessage, OutputSnapshotEnvelope, OutputStream, ServerContext,
};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};

use super::Server;

impl Server {
    pub(super) fn start_event_tail(
        &self,
        mut receiver: mpsc::Receiver<EventSubscriptionMessage>,
        context: ServerContext,
    ) {
        let server = self.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            loop {
                let message = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    message = receiver.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                };
                let tool_call_id = match &message {
                    EventSubscriptionMessage::Event { event } => match &event.payload {
                        EventPayload::ToolCallStarted { start } => Some(start.tool_call_id),
                        _ => None,
                    },
                    EventSubscriptionMessage::Gap { .. } => None,
                };
                if context
                    .notify("events.subscription", &message)
                    .await
                    .is_err()
                {
                    return;
                }
                if let Some(tool_call_id) = tool_call_id {
                    server.start_output_tail(tool_call_id, context.clone());
                }
            }
        });
    }

    pub(super) fn start_output_tail(
        &self,
        tool_call_id: cookie_agent_protocol::ToolCallId,
        context: ServerContext,
    ) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let shutdown = context.shutdown();
            for _ in 0..10 {
                let stdout = engine.subscribe_tool_output(tool_call_id, OutputStream::Stdout);
                let stderr = engine.subscribe_tool_output(tool_call_id, OutputStream::Stderr);
                if stdout.is_some() || stderr.is_some() {
                    if let Some((snapshot, receiver)) = stdout {
                        tokio::spawn(forward_output(
                            OutputStream::Stdout,
                            snapshot,
                            receiver,
                            context.clone(),
                        ));
                    }
                    if let Some((snapshot, receiver)) = stderr {
                        tokio::spawn(forward_output(
                            OutputStream::Stderr,
                            snapshot,
                            receiver,
                            context,
                        ));
                    }
                    return;
                }
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sleep(Duration::from_millis(5)) => {}
                }
            }
        });
    }
}

async fn forward_output(
    stream: OutputStream,
    snapshot: cookie_agent_protocol::OutputSnapshot,
    mut receiver: mpsc::Receiver<OutputMessage>,
    context: ServerContext,
) {
    let shutdown = context.shutdown();
    let held_delta = match receiver.try_recv() {
        Ok(OutputMessage::Gap(gap)) => {
            if context
                .notify("events.tool_output_gap", &gap)
                .await
                .is_err()
            {
                return;
            }
            None
        }
        Ok(OutputMessage::Delta(delta)) => Some(delta),
        Err(_) => None,
    };
    if context
        .notify(
            "events.tool_output_snapshot",
            &OutputSnapshotEnvelope { stream, snapshot },
        )
        .await
        .is_err()
    {
        return;
    }
    if let Some(delta) = held_delta
        && context
            .notify("events.tool_output_delta", &delta)
            .await
            .is_err()
    {
        return;
    }
    loop {
        let message = tokio::select! {
            _ = shutdown.cancelled() => return,
            message = receiver.recv() => match message {
                Some(message) => message,
                None => return,
            },
        };
        let result = match message {
            OutputMessage::Delta(delta) => context.notify("events.tool_output_delta", &delta).await,
            OutputMessage::Gap(gap) => context.notify("events.tool_output_gap", &gap).await,
        };
        if result.is_err() {
            return;
        }
    }
}
