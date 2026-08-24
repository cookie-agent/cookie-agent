//! Protocol-10 coherent runtime and provider mutation RPCs.

use crate::{
    ProviderConnectParams, ProviderConnectResult, ProviderDisconnectParams,
    ProviderDisconnectResult, RuntimeSnapshotGetParams, RuntimeSnapshotResult,
};
use serde_json::Value;

use super::{
    Client, ClientError, SensitiveJson, record_provider_connect_wipe, send_sensitive_command,
};

struct ProviderConnectGuard(ProviderConnectParams);

impl Drop for ProviderConnectGuard {
    fn drop(&mut self) {
        record_provider_connect_wipe();
    }
}

impl Client {
    pub async fn runtime_snapshot(&self) -> Result<RuntimeSnapshotResult, ClientError> {
        self.call(
            crate::RUNTIME_SNAPSHOT_GET_METHOD,
            &RuntimeSnapshotGetParams {},
        )
        .await
    }

    pub fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> impl std::future::Future<Output = Result<ProviderConnectResult, ClientError>> + '_ {
        let params = ProviderConnectGuard(params);
        async move {
            let value = provider_connect_value(&params.0)?;
            let result =
                send_sensitive_command(&self.commands, crate::PROVIDER_CONNECT_METHOD, value)
                    .await?;
            Ok(serde_json::from_value(result)?)
        }
    }

    pub async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult, ClientError> {
        self.call(crate::PROVIDER_DISCONNECT_METHOD, &params).await
    }
}

fn provider_connect_value(params: &ProviderConnectParams) -> Result<SensitiveJson, ClientError> {
    let setup_values = serde_json::to_value(&params.setup_values)?;
    let provider_id = serde_json::to_value(&params.provider_id)?;
    let expected_catalog_revision = serde_json::to_value(&params.expected_catalog_revision)?;
    let auth_method = serde_json::to_value(&params.auth_method)?;
    let client_connect_id = serde_json::to_value(&params.client_connect_id)?;
    let mut value = SensitiveJson::object();
    let object = value.object_mut();
    object.insert("provider_id".into(), provider_id);
    object.insert(
        "expected_catalog_revision".into(),
        expected_catalog_revision,
    );
    object.insert("setup_values".into(), setup_values);
    object.insert("auth_method".into(), auth_method);
    object.insert("auth_values".into(), Value::Object(serde_json::Map::new()));
    object.insert("client_connect_id".into(), client_connect_id);
    let auth_values = object
        .get_mut("auth_values")
        .and_then(Value::as_object_mut)
        .expect("auth values were initialized as an object");
    for field in params.auth_values.field_names() {
        let id = crate::AuthFieldName::new(field.to_owned())
            .expect("protocol credential field names are validated");
        if let Some(secret) = params.auth_values.get(&id) {
            auth_values.insert(field.to_owned(), Value::String(secret.to_owned()));
        }
    }
    Ok(value)
}
