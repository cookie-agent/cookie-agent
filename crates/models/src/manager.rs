//! Atomic immutable provider/model snapshots and retained frozen bindings.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use cookie_agent_identity::ProviderId;
use thiserror::Error;

use crate::{
    Catalog, CredentialConnectReceipt, CredentialConnectRequest, CredentialSnapshot,
    CredentialStore, CredentialStoreError, FrozenModelBinding, ModelBuildError, ModelSet,
    ModelSetError, ProviderDefinition, ResolvedModel, Sha256Digest, build_model_set,
};

#[derive(Clone)]
pub struct ModelSnapshot {
    model_set: Arc<ModelSet>,
    revision: String,
    generated_at: String,
    catalog_revision: String,
}

impl ModelSnapshot {
    #[must_use]
    pub fn model_set(&self) -> &Arc<ModelSet> {
        &self.model_set
    }
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
    #[must_use]
    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }
    #[must_use]
    pub fn catalog_revision(&self) -> &str {
        &self.catalog_revision
    }

    /// Resolves a frozen binding against this exact executable snapshot.
    pub fn resolve(&self, binding: &FrozenModelBinding) -> Result<ResolvedModel, ModelSetError> {
        self.model_set.resolve(binding)
    }
}

impl fmt::Debug for ModelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSnapshot")
            .field("revision", &self.revision)
            .field("generated_at", &self.generated_at)
            .field("catalog_revision", &self.catalog_revision)
            .field(
                "models",
                &self
                    .model_set
                    .entries()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

pub struct ModelSetManager {
    providers: BTreeMap<ProviderId, ProviderDefinition>,
    catalog: Arc<Catalog>,
    credentials: CredentialStore,
    current: ArcSwap<ModelSnapshot>,
    retained: Mutex<BTreeMap<Sha256Digest, Vec<Arc<ModelSnapshot>>>>,
    refresh: Mutex<()>,
}

impl ModelSetManager {
    pub fn new(
        providers: BTreeMap<ProviderId, ProviderDefinition>,
        catalog: Arc<Catalog>,
        credentials: CredentialStore,
    ) -> Result<Self, ModelSetManagerError> {
        let credential_snapshot = credentials.snapshot()?;
        let initial = Arc::new(build_snapshot(&providers, &catalog, &credential_snapshot)?);
        let retained = [(
            initial.model_set.fingerprint().clone(),
            vec![Arc::clone(&initial)],
        )]
        .into_iter()
        .collect();
        Ok(Self {
            providers,
            catalog,
            credentials,
            current: ArcSwap::from(initial),
            retained: Mutex::new(retained),
            refresh: Mutex::new(()),
        })
    }

    #[must_use]
    pub fn current(&self) -> Arc<ModelSnapshot> {
        self.current.load_full()
    }

    pub fn refresh(&self) -> Result<Arc<ModelSnapshot>, ModelSetManagerError> {
        let _guard = self
            .refresh
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let credentials = self.credentials.snapshot()?;
        let candidate = Arc::new(build_snapshot(
            &self.providers,
            &self.catalog,
            &credentials,
        )?);
        Ok(self.publish(candidate))
    }

    pub fn connect(
        &self,
        request: &CredentialConnectRequest,
    ) -> Result<CredentialConnectReceipt, ModelSetManagerError> {
        let _guard = self
            .refresh
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let provider_id = &request.provider_id;
        let Some(definition) = self.providers.get(provider_id) else {
            return Err(ModelSetManagerError::UnknownProvider);
        };
        let ProviderDefinition::ModelsDev(provider) = definition else {
            return Err(ModelSetManagerError::ProviderDoesNotUseCredentialStore);
        };
        if !matches!(provider.auth, crate::AuthDefinition::CredentialStore) {
            return Err(ModelSetManagerError::ProviderDoesNotUseCredentialStore);
        }
        if request.catalog_revision != format!("sha256:{}", crate::MODELS_DEV_ARTIFACT_SHA256) {
            return Err(ModelSetManagerError::CatalogRevisionConflict);
        }
        let catalog_provider = self
            .catalog
            .providers()
            .get(provider_id.as_str())
            .ok_or(ModelSetManagerError::UnknownProvider)?;
        let missing = catalog_provider
            .credential_fields
            .iter()
            .filter(|field| !request.credentials.contains_key(*field))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(ModelSetManagerError::MissingCredentials(missing));
        }
        if request.credentials.len() != catalog_provider.credential_fields.len() {
            return Err(ModelSetManagerError::InvalidCredentials);
        }
        let mut candidate_error = None;
        let outcome = self.credentials.connect_with(request, |credentials| {
            match build_snapshot(&self.providers, &self.catalog, credentials) {
                Ok(snapshot) => {
                    let revision = snapshot.revision.clone();
                    Ok((revision, Arc::new(snapshot)))
                }
                Err(error) => {
                    candidate_error = Some(error);
                    Err(CredentialStoreError::CandidateRejected)
                }
            }
        });
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(CredentialStoreError::CandidateRejected) => {
                return Err(candidate_error.unwrap_or(ModelSetManagerError::CandidateRejected));
            }
            Err(error) => return Err(ModelSetManagerError::Credentials(error)),
        };
        let candidate = if let Some(candidate) = outcome.candidate {
            candidate
        } else {
            let credentials = self.credentials.snapshot()?;
            Arc::new(build_snapshot(
                &self.providers,
                &self.catalog,
                &credentials,
            )?)
        };
        self.publish(candidate);
        Ok(outcome.receipt)
    }

    #[must_use]
    pub fn snapshot(&self, fingerprint: &Sha256Digest) -> Option<Arc<ModelSnapshot>> {
        let current = self.current();
        if current.model_set.fingerprint() == fingerprint {
            return Some(current);
        }
        self.retained
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(fingerprint)
            .and_then(|snapshots| snapshots.last())
            .cloned()
    }

    fn publish(&self, candidate: Arc<ModelSnapshot>) -> Arc<ModelSnapshot> {
        let mut retained = self
            .retained
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        retained
            .entry(candidate.model_set.fingerprint().clone())
            .or_default()
            .push(Arc::clone(&candidate));
        self.current.store(Arc::clone(&candidate));
        candidate
    }
}

impl fmt::Debug for ModelSetManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSetManager")
            .field("current", &self.current())
            .finish_non_exhaustive()
    }
}

fn build_snapshot(
    providers: &BTreeMap<ProviderId, ProviderDefinition>,
    catalog: &Catalog,
    credentials: &CredentialSnapshot,
) -> Result<ModelSnapshot, ModelSetManagerError> {
    let model_set = Arc::new(build_model_set(providers, catalog, Some(credentials))?);
    Ok(ModelSnapshot {
        revision: format!("sha256:{}", model_set.fingerprint().as_str()),
        model_set,
        generated_at: jiff::Timestamp::now().to_string(),
        catalog_revision: format!("sha256:{}", crate::MODELS_DEV_ARTIFACT_SHA256),
    })
}

#[derive(Debug, Error)]
pub enum ModelSetManagerError {
    #[error("unknown configured models.dev provider")]
    UnknownProvider,
    #[error("provider does not use credential_store")]
    ProviderDoesNotUseCredentialStore,
    #[error("catalog revision does not match the pinned snapshot")]
    CatalogRevisionConflict,
    #[error("provider credentials do not match the pinned recipe")]
    InvalidCredentials,
    #[error("provider credentials are missing required fields")]
    MissingCredentials(Vec<String>),
    #[error("candidate model snapshot was rejected")]
    CandidateRejected,
    #[error("obsolete_model_fingerprint")]
    ObsoleteModelFingerprint,
    #[error("credential storage failed: {0}")]
    Credentials(#[from] CredentialStoreError),
    #[error("provider/model construction failed: {0}")]
    Models(#[from] ModelBuildError),
    #[error("model set validation failed: {0}")]
    Set(#[from] ModelSetError),
}
