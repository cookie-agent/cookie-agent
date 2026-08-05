use cookie_agent_protocol::{
    ProviderConnectParams, ProviderConnectResult, ProviderDisconnectParams,
    ProviderDisconnectResult,
};

use crate::{rpc::RpcFault, service::Server};

impl Server {
    pub(crate) fn connect_provider(
        &self,
        request: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, RpcFault> {
        self.engine
            .connect_provider(request.clone())
            .map_err(|error| RpcFault::provider_connect(&request, error))
    }

    pub(crate) fn disconnect_provider(
        &self,
        request: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, RpcFault> {
        self.engine
            .disconnect_provider(request.clone())
            .map_err(|error| RpcFault::provider_disconnect(&request, error))
    }
}
