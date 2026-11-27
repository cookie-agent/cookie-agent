//! The daemon's side of a runtime's `net/*` requests
//! (`docs/site/specs/forwards.md` section 4): listening on and connecting
//! from the runtime's machine for a forward. The bytes travel as
//! notifications, which [`super::super::NetRelayHandler`] receives.

use std::time::Duration;

use cookie_agent_protocol::runtime_protocol::{
    NetConnectParams, NetConnectResult, NetListenParams, NetListenResult, NetListenerRef,
    NetProtocol, method,
};

use super::RuntimeClient;

/// How long a `net/*` request may take: a connect waits up to 10 seconds on
/// the runtime.
const NET_REQUEST_DEADLINE: Duration = Duration::from_secs(30);

impl RuntimeClient {
    /// Whether the runtime takes part in forwards.
    pub(crate) fn net(&self) -> bool {
        self.inner.net.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn net_request<P, R>(&self, method: &str, params: &P) -> Result<R, String>
    where
        P: serde::Serialize + Sync,
        R: serde::de::DeserializeOwned,
    {
        self.inner
            .request_within(method, params, NET_REQUEST_DEADLINE)
            .await
            .map_err(|error| match error.detail() {
                Some(detail) => detail.to_owned(),
                None => error.to_string(),
            })
    }

    /// `net/listen`: the listener and the port it got.
    pub(crate) async fn net_listen(
        &self,
        protocol: NetProtocol,
        bind: &str,
        port: u16,
    ) -> Result<NetListenResult, String> {
        self.net_request(
            method::NET_LISTEN,
            &NetListenParams {
                protocol,
                bind: bind.to_owned(),
                port,
            },
        )
        .await
    }

    /// `net/unlisten`.
    pub(crate) async fn net_unlisten(&self, listener: u64) {
        let _: Result<cookie_agent_protocol::runtime_protocol::Empty, String> = self
            .net_request(method::NET_UNLISTEN, &NetListenerRef { listener })
            .await;
    }

    /// `net/connect`: the connection.
    pub(crate) async fn net_connect(
        &self,
        protocol: NetProtocol,
        host: &str,
        port: u16,
    ) -> Result<u64, String> {
        let result: NetConnectResult = self
            .net_request(
                method::NET_CONNECT,
                &NetConnectParams {
                    protocol,
                    host: host.to_owned(),
                    port,
                },
            )
            .await?;
        Ok(result.connection)
    }
}
