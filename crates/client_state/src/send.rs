//! Sending a message the composer holds: it reaches the daemon, or it goes
//! back to the draft it came from with why. A message never vanishes: one
//! sent while the client has no connection, or whose answer a closing
//! connection lost (the daemon restarting, say), is the client's to put
//! back.

use cookie_agent_protocol::{SessionSubmitInputParams, SessionSubmitInputResult};
use cookie_agent_protocol_transport::{Client, ClientError};

/// Why a message did not reach the daemon, or may not have.
#[derive(Debug)]
pub enum Unsent {
    /// There was no connection to send it on: it was not sent.
    NotConnected,
    /// The daemon refused it, or the connection closed before it answered.
    Failed(ClientError),
}

impl Unsent {
    /// What the composer says of the message it holds again.
    pub fn describe(&self, error: impl FnOnce(&ClientError) -> String) -> String {
        match self {
            Self::NotConnected => {
                "not connected to the daemon: the message was not sent".to_owned()
            }
            Self::Failed(failure) => error(failure),
        }
    }
}

/// Submits `params` over `client`, `None` while the client has no
/// connection. An `Err` hands the message back: it was not sent, or its
/// answer never came.
pub async fn submit_input(
    client: Option<&Client>,
    params: SessionSubmitInputParams,
) -> Result<SessionSubmitInputResult, Unsent> {
    let client = client.ok_or(Unsent::NotConnected)?;
    client.submit_input(params).await.map_err(Unsent::Failed)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use cookie_agent_protocol::{
        ClientRunId, InputDelivery, MessageFrame, MessageStream as _, SessionId,
        SessionSubmitInputParams, in_process_pair,
    };
    use cookie_agent_protocol_transport::Client;

    use super::{Unsent, submit_input};

    fn params() -> SessionSubmitInputParams {
        SessionSubmitInputParams {
            session_id: SessionId::new_v7(),
            content: "say afterwards".into(),
            delivery: InputDelivery::Steer,
            selection: None,
            client_run_id: ClientRunId::new("client-run").unwrap(),
            reset_fallback: false,
        }
    }

    #[tokio::test]
    async fn a_message_sent_with_no_connection_comes_back() {
        assert!(matches!(
            submit_input(None, params()).await,
            Err(Unsent::NotConnected)
        ));
    }

    #[tokio::test]
    async fn a_message_whose_connection_closes_before_the_answer_comes_back() {
        let (client_end, mut daemon_end) = in_process_pair(8);
        let client = Client::connect_stream(client_end);
        // The daemon reads the message, then its connection closes without
        // an answer, as a restarting daemon's does.
        let daemon = tokio::spawn(async move {
            let request = daemon_end.recv().await.unwrap().unwrap();
            drop(daemon_end);
            request
        });
        let unsent = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            submit_input(Some(&client), params()),
        )
        .await
        .expect("the send ends with the connection")
        .expect_err("no answer came");
        assert!(matches!(unsent, Unsent::Failed(_)), "{unsent:?}");
        let MessageFrame::Text(request) = daemon.await.unwrap() else {
            panic!("a text frame");
        };
        assert!(request.contains("say afterwards"), "{request}");
    }
}
