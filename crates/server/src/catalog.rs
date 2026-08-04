use cookie_agent_models::{
    AuthDefinition, Catalog, CredentialConnectRequest, CredentialStoreError, ModelSetManagerError,
    ProviderDefinition,
};
use cookie_agent_protocol::{
    AgentListResult, CatalogErrorCode, CatalogModelListParams, CatalogModelListResult,
    CatalogProvider, CatalogProviderListResult, CatalogRevision, CatalogSnapshot, ClientConnectId,
    CredentialFieldName, ModelListErrorCode, ModelListResult, ProviderConnectErrorCode,
    ProviderConnectParams, ProviderConnectResult, ProviderConnection, SnapshotRevision,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{rpc::RpcFault, service::Server};

impl Server {
    pub(crate) fn list_models(&self) -> Result<ModelListResult, RpcFault> {
        let snapshot = self.model_manager.current();
        Ok(ModelListResult {
            revision: SnapshotRevision::new(snapshot.revision())
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            generated_at: snapshot
                .generated_at()
                .parse()
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            catalog_revision: CatalogRevision::new(snapshot.catalog_revision())
                .map_err(|_| RpcFault::model_list(ModelListErrorCode::ModelSnapshotInvalid))?,
            models: project_value(&snapshot.model_set().descriptors())?,
        })
    }

    pub(crate) fn list_agents(&self) -> Result<AgentListResult, RpcFault> {
        Ok(self.engine.list_agents())
    }

    pub(crate) fn list_catalog_providers(&self) -> Result<CatalogProviderListResult, RpcFault> {
        let providers = self
            .configuration
            .runtime
            .providers
            .iter()
            .filter_map(|(provider_id, definition)| match definition {
                ProviderDefinition::ModelsDev(provider)
                    if matches!(provider.auth, AuthDefinition::CredentialStore) =>
                {
                    self.catalog.providers().get(provider_id.as_str())
                }
                ProviderDefinition::ModelsDev(_) | ProviderDefinition::Explicit(_) => None,
            })
            .map(project_catalog_provider)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CatalogProviderListResult {
            snapshot: catalog_snapshot(&self.catalog)?,
            providers,
        })
    }

    pub(crate) fn list_catalog_models(
        &self,
        request: &CatalogModelListParams,
    ) -> Result<CatalogModelListResult, RpcFault> {
        let models = self
            .catalog
            .models()
            .iter()
            .filter(|model| {
                request
                    .provider_id
                    .as_ref()
                    .is_none_or(|provider| model.provider_id == provider.as_str())
            })
            // The vendored source retains upstream records with zero or
            // contradictory limits. They are not valid protocol-v7 catalog
            // descriptors and are excluded rather than emitting invalid wire.
            .filter_map(|model| project_value(model).ok())
            .collect::<Vec<_>>();
        Ok(CatalogModelListResult {
            snapshot: catalog_snapshot(&self.catalog)?,
            models,
        })
    }

    pub(crate) fn connect_provider(
        &self,
        request: ProviderConnectParams,
    ) -> Result<ProviderConnectResult, RpcFault> {
        let manager_request = into_manager_connect_request(request);
        let receipt = self
            .model_manager
            .connect(&manager_request)
            .map_err(|error| provider_connect_fault(&manager_request, error))?;
        Ok(ProviderConnectResult {
            client_connect_id: ClientConnectId::new(receipt.client_connect_id)
                .map_err(|_| RpcFault::internal())?,
            connection: ProviderConnection {
                provider_id: receipt.provider_id,
                credential_fields: receipt
                    .credential_fields
                    .into_iter()
                    .map(CredentialFieldName::new)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| RpcFault::internal())?,
                connected_at: receipt
                    .connected_at
                    .parse()
                    .map_err(|_| RpcFault::internal())?,
                catalog_revision: CatalogRevision::new(receipt.catalog_revision)
                    .map_err(|_| RpcFault::internal())?,
            },
            model_revision: SnapshotRevision::new(receipt.model_revision)
                .map_err(|_| RpcFault::internal())?,
        })
    }
}

fn project_catalog_provider(
    provider: &cookie_agent_models::CatalogProvider,
) -> Result<CatalogProvider, RpcFault> {
    let mut credential_fields = provider
        .credential_fields
        .iter()
        .cloned()
        .map(CredentialFieldName::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RpcFault::internal())?;
    credential_fields.sort();
    Ok(CatalogProvider {
        id: cookie_agent_protocol::CatalogIdentifier::new(provider.id.clone())
            .map_err(|_| RpcFault::internal())?,
        name: cookie_agent_protocol::CatalogText::new(provider.name.clone())
            .map_err(|_| RpcFault::internal())?,
        credential_fields,
        npm: cookie_agent_protocol::CatalogText::new(provider.npm.clone())
            .map_err(|_| RpcFault::internal())?,
        api: provider
            .api
            .clone()
            .map(cookie_agent_protocol::CatalogText::new)
            .transpose()
            .map_err(|_| RpcFault::internal())?,
        documentation_url: cookie_agent_protocol::CatalogText::new(
            provider.documentation_url.clone(),
        )
        .map_err(|_| RpcFault::internal())?,
    })
}

pub(crate) fn into_manager_connect_request(
    request: ProviderConnectParams,
) -> CredentialConnectRequest {
    CredentialConnectRequest {
        client_connect_id: request.client_connect_id.to_string(),
        provider_id: request.provider_id,
        catalog_revision: request.catalog_revision.to_string(),
        credentials: request
            .credentials
            .values
            .into_iter()
            .map(|(field, value)| (field.to_string(), value))
            .collect(),
    }
}

fn project_value<T, U>(value: &T) -> Result<U, RpcFault>
where
    T: Serialize,
    U: DeserializeOwned,
{
    serde_json::to_value(value)
        .map_err(|_| RpcFault::internal())
        .and_then(|value| serde_json::from_value(value).map_err(|_| RpcFault::internal()))
}

fn catalog_snapshot(catalog: &Catalog) -> Result<CatalogSnapshot, RpcFault> {
    let snapshot = catalog.snapshot();
    Ok(CatalogSnapshot {
        revision: CatalogRevision::new(snapshot.revision)
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
        source: cookie_agent_protocol::CatalogText::new(snapshot.source)
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
        fetched_at: snapshot
            .fetched_at
            .parse()
            .map_err(|_| RpcFault::catalog(CatalogErrorCode::CatalogSnapshotInvalid, None))?,
    })
}

fn provider_connect_fault(
    request: &CredentialConnectRequest,
    error: ModelSetManagerError,
) -> RpcFault {
    let (code, missing_credential_fields) = match error {
        ModelSetManagerError::UnknownProvider => {
            (ProviderConnectErrorCode::UnknownProvider, Vec::new())
        }
        ModelSetManagerError::ProviderDoesNotUseCredentialStore => {
            (ProviderConnectErrorCode::UnsupportedProvider, Vec::new())
        }
        ModelSetManagerError::CatalogRevisionConflict => (
            ProviderConnectErrorCode::CatalogRevisionConflict,
            Vec::new(),
        ),
        ModelSetManagerError::MissingCredentials(fields) => {
            let fields = fields
                .into_iter()
                .map(CredentialFieldName::new)
                .collect::<Result<Vec<_>, _>>();
            let Ok(fields) = fields else {
                return RpcFault::internal();
            };
            (ProviderConnectErrorCode::MissingCredential, fields)
        }
        ModelSetManagerError::InvalidCredentials
        | ModelSetManagerError::Credentials(CredentialStoreError::InvalidRequest) => {
            (ProviderConnectErrorCode::InvalidCredential, Vec::new())
        }
        ModelSetManagerError::Credentials(CredentialStoreError::IdempotencyConflict) => {
            (ProviderConnectErrorCode::IdempotencyConflict, Vec::new())
        }
        ModelSetManagerError::Credentials(
            CredentialStoreError::HomeUnavailable
            | CredentialStoreError::UnsupportedPlatform
            | CredentialStoreError::UnsafePath
            | CredentialStoreError::InvalidStore
            | CredentialStoreError::Io(_)
            | CredentialStoreError::Json(_)
            | CredentialStoreError::Clock,
        ) => (
            ProviderConnectErrorCode::CredentialStorageFailed,
            Vec::new(),
        ),
        ModelSetManagerError::Credentials(CredentialStoreError::CandidateRejected)
        | ModelSetManagerError::CandidateRejected
        | ModelSetManagerError::Models(_)
        | ModelSetManagerError::Set(_)
        | ModelSetManagerError::ObsoleteModelFingerprint => return RpcFault::internal(),
    };
    let client_connect_id = ClientConnectId::new(request.client_connect_id.clone())
        .unwrap_or_else(|_| ClientConnectId::new("invalid-connect-id").expect("static ID"));
    RpcFault::provider_connect_parts(
        &request.provider_id,
        &client_connect_id,
        code,
        missing_credential_fields,
    )
}
