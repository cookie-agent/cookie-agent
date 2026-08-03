//! Atomic immutable runtime model snapshots and frozen-binding retention.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    Catalog, CatalogBuildError, ConfigurationFingerprint, ConfiguredModel,
    CredentialConnectReceipt, CredentialConnectRequest, CredentialSnapshot, CredentialStore,
    CredentialStoreError, FrozenModelBinding, ModelBuildError, ModelEntry, ModelSet, ModelSetError,
    build_model_set,
};

/// One complete immutable model state published atomically.
#[derive(Clone)]
pub struct ModelSnapshot {
    model_set: Arc<ModelSet>,
    revision: String,
    generated_at: String,
    catalog_revision: Option<String>,
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
    pub fn catalog_revision(&self) -> Option<&str> {
        self.catalog_revision.as_deref()
    }
}

impl fmt::Debug for ModelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSnapshot")
            .field("revision", &self.revision)
            .field("generated_at", &self.generated_at)
            .field("catalog_revision", &self.catalog_revision)
            .field("aliases", &self.model_set.aliases().collect::<Vec<_>>())
            .finish()
    }
}

/// Thread-safe atomic model-set manager. Wrap this value in `Arc` for sharing.
pub struct ModelSetManager {
    static_models: BTreeMap<String, ConfiguredModel>,
    catalog: Arc<Catalog>,
    credentials: CredentialStore,
    current: ArcSwap<ModelSnapshot>,
    retained: Mutex<BTreeMap<ConfigurationFingerprint, Arc<ModelSnapshot>>>,
    refresh: Mutex<()>,
}

impl ModelSetManager {
    /// Loads credentials, validates a complete initial candidate, and publishes once.
    pub fn new(
        static_models: BTreeMap<String, ConfiguredModel>,
        catalog: Arc<Catalog>,
        credentials: CredentialStore,
    ) -> Result<Self, ModelSetManagerError> {
        let credential_snapshot = credentials.snapshot()?;
        let initial = Arc::new(build_snapshot(
            &static_models,
            &catalog,
            &credential_snapshot,
        )?);
        let retained = [(
            initial.model_set.fingerprint().clone(),
            Arc::clone(&initial),
        )]
        .into_iter()
        .collect();
        Ok(Self {
            static_models,
            catalog,
            credentials,
            current: ArcSwap::from(initial),
            retained: Mutex::new(retained),
            refresh: Mutex::new(()),
        })
    }

    /// Returns the current immutable snapshot with one atomic load.
    #[must_use]
    pub fn current(&self) -> Arc<ModelSnapshot> {
        self.current.load_full()
    }

    /// Rebuilds from durable credentials and publishes only a fully validated candidate.
    pub fn refresh(&self) -> Result<Arc<ModelSnapshot>, ModelSetManagerError> {
        let _guard = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let credentials = self.credentials.snapshot()?;
        let candidate = Arc::new(build_snapshot(
            &self.static_models,
            &self.catalog,
            &credentials,
        )?);
        self.publish(candidate)
    }

    /// Durably stores an idempotent connection, then atomically publishes its validated set.
    pub fn connect(
        &self,
        request: &CredentialConnectRequest,
    ) -> Result<CredentialConnectReceipt, ModelSetManagerError> {
        let _guard = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if request.catalog_revision != self.catalog.revision() {
            return Err(ModelSetManagerError::CatalogRevisionConflict);
        }
        if !self.catalog.is_known_provider(&request.provider_id) {
            return Err(ModelSetManagerError::UnknownProvider);
        }
        if !self.catalog.is_supported_provider(&request.provider_id) {
            return Err(ModelSetManagerError::UnsupportedProvider);
        }
        let provider = &self.catalog.providers()[&request.provider_id];
        if request.credentials.len() != 1
            || request.credentials.keys().any(|field| {
                !provider
                    .credential_fields
                    .iter()
                    .any(|known| known == field)
            })
        {
            return Err(ModelSetManagerError::InvalidCredentials);
        }

        let mut candidate_error = None;
        let outcome = self.credentials.connect_with(request, |credentials| {
            match build_snapshot(&self.static_models, &self.catalog, credentials) {
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
        if let Some(candidate) = outcome.candidate {
            self.publish(candidate)?;
        } else {
            // Another process may have completed the original request. Reread under a
            // fresh lock so this process converges without changing the original receipt.
            let credentials = self.credentials.snapshot()?;
            let candidate = Arc::new(build_snapshot(
                &self.static_models,
                &self.catalog,
                &credentials,
            )?);
            self.publish(candidate)?;
        }
        Ok(outcome.receipt)
    }

    /// Resolves a frozen binding through current or retained fingerprint state.
    pub fn resolve_frozen(
        &self,
        binding: &FrozenModelBinding,
    ) -> Result<ModelEntry, ModelSetManagerError> {
        // Serialize resolution with publication. A resolver that acquired this guard
        // before rotation may finish with its already-acquired adapter; every resolver
        // after publication observes only the rebound/current credential generation.
        let _guard = self
            .refresh
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.current();
        if current.model_set.fingerprint() == &binding.configuration_fingerprint {
            return current
                .model_set
                .resolve(binding)
                .cloned()
                .map_err(ModelSetManagerError::Set);
        }
        let retained = self
            .retained
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retained
            .get(&binding.configuration_fingerprint)
            .ok_or(ModelSetManagerError::RetainedSnapshotNotFound)?
            .model_set
            .resolve(binding)
            .cloned()
            .map_err(ModelSetManagerError::Set)
    }

    fn publish(
        &self,
        candidate: Arc<ModelSnapshot>,
    ) -> Result<Arc<ModelSnapshot>, ModelSetManagerError> {
        let fingerprint = candidate.model_set.fingerprint().clone();
        let mut retained = self
            .retained
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut rebound = retained
            .iter()
            .filter_map(|(retained_fingerprint, snapshot)| {
                if retained_fingerprint == &fingerprint {
                    None
                } else {
                    rebind_retained_snapshot(snapshot, &candidate)
                        .map(|snapshot| (retained_fingerprint.clone(), snapshot))
                }
            })
            .collect::<BTreeMap<_, _>>();
        rebound.insert(fingerprint, Arc::clone(&candidate));
        *retained = rebound;
        self.current.store(Arc::clone(&candidate));
        Ok(candidate)
    }
}

impl fmt::Debug for ModelSetManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelSetManager")
            .field("current", &self.current())
            .field("catalog_revision", &self.catalog.revision())
            .finish_non_exhaustive()
    }
}

fn build_snapshot(
    static_models: &BTreeMap<String, ConfiguredModel>,
    catalog: &Catalog,
    credentials: &CredentialSnapshot,
) -> Result<ModelSnapshot, ModelSetManagerError> {
    let static_set = build_model_set(static_models)?;
    let mut entries = static_set
        .entries()
        .map(|(alias, entry)| (alias.to_owned(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut has_catalog = false;
    for connection in credentials.connections().values() {
        if connection.catalog_revision != catalog.revision() {
            return Err(ModelSetManagerError::CatalogRevisionConflict);
        }
        for model in catalog
            .models()
            .iter()
            .filter(|model| model.provider_id == connection.provider_id)
        {
            if catalog.recipe(model).is_err() {
                continue;
            }
            let entry = catalog.build_generated(model, &connection.credentials)?;
            let alias = model.alias();
            if entries.insert(alias.clone(), entry).is_some() {
                return Err(ModelSetManagerError::StaticAliasCollision(alias));
            }
            has_catalog = true;
        }
    }
    let fingerprint = snapshot_fingerprint(
        static_set.fingerprint(),
        has_catalog.then(|| catalog.revision()),
        &entries,
    )?;
    let set = Arc::new(ModelSet::new(entries, fingerprint.clone())?);
    Ok(ModelSnapshot {
        model_set: set,
        revision: format!("sha256:{}", fingerprint.as_str()),
        generated_at: timestamp()?,
        catalog_revision: has_catalog.then(|| catalog.revision().to_owned()),
    })
}

fn rebind_retained_snapshot(
    retained: &ModelSnapshot,
    candidate: &ModelSnapshot,
) -> Option<Arc<ModelSnapshot>> {
    let entries = retained
        .model_set
        .entries()
        .map(|(alias, retained_entry)| {
            let replacement = candidate.model_set.get(alias)?;
            if replacement.descriptor() != retained_entry.descriptor()
                || replacement.defaults() != retained_entry.defaults()
                || replacement.behavior_fingerprint() != retained_entry.behavior_fingerprint()
            {
                return None;
            }
            Some((alias.to_owned(), replacement.clone()))
        })
        .collect::<Option<Vec<_>>>()?;
    let model_set = ModelSet::new(entries, retained.model_set.fingerprint().clone()).ok()?;
    Some(Arc::new(ModelSnapshot {
        model_set: Arc::new(model_set),
        revision: retained.revision.clone(),
        generated_at: candidate.generated_at.clone(),
        catalog_revision: retained.catalog_revision.clone(),
    }))
}

fn snapshot_fingerprint(
    static_fingerprint: &ConfigurationFingerprint,
    catalog_revision: Option<&str>,
    entries: &BTreeMap<String, ModelEntry>,
) -> Result<ConfigurationFingerprint, ModelSetManagerError> {
    #[derive(Serialize)]
    struct SafeEntry<'a> {
        alias: &'a str,
        descriptor: &'a oven_sdk::LanguageModelDescriptor,
        defaults: &'a crate::RequestDefaults,
        behavior_fingerprint: &'a ConfigurationFingerprint,
    }
    #[derive(Serialize)]
    struct SafeSnapshot<'a> {
        static_fingerprint: &'a ConfigurationFingerprint,
        catalog_revision: Option<&'a str>,
        entries: Vec<SafeEntry<'a>>,
    }
    let safe = SafeSnapshot {
        static_fingerprint,
        catalog_revision,
        entries: entries
            .iter()
            .map(|(alias, entry)| SafeEntry {
                alias,
                descriptor: entry.descriptor(),
                defaults: entry.defaults(),
                behavior_fingerprint: entry.behavior_fingerprint(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&safe).map_err(ModelSetManagerError::Canonical)?;
    ConfigurationFingerprint::new(format!("{:x}", Sha256::digest(bytes)))
        .map_err(ModelSetManagerError::Set)
}

fn timestamp() -> Result<String, ModelSetManagerError> {
    Ok(jiff::Timestamp::now().to_string())
}

/// Atomic model manager errors contain no credentials or secret-bearing requests.
#[derive(Error)]
pub enum ModelSetManagerError {
    #[error("unknown catalog provider")]
    UnknownProvider,
    #[error("known catalog provider is unsupported")]
    UnsupportedProvider,
    #[error("catalog revision does not match the pinned snapshot")]
    CatalogRevisionConflict,
    #[error("provider credentials do not match the reviewed recipe")]
    InvalidCredentials,
    #[error("static model alias collides with generated alias `{0}`")]
    StaticAliasCollision(String),
    #[error("candidate model snapshot was rejected")]
    CandidateRejected,
    #[error("retained model snapshot was not found")]
    RetainedSnapshotNotFound,
    #[error("credential storage failed: {0}")]
    Credentials(#[from] CredentialStoreError),
    #[error("catalog model construction failed: {0}")]
    Catalog(#[from] CatalogBuildError),
    #[error("static model construction failed: {0}")]
    Models(#[from] ModelBuildError),
    #[error("model set validation failed: {0}")]
    Set(#[from] ModelSetError),
    #[error("model snapshot canonicalization failed")]
    Canonical(#[source] serde_json::Error),
    #[error("system clock is before the Unix epoch")]
    Clock,
}

impl fmt::Debug for ModelSetManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ModelSetManagerError")
            .field(&self.to_string())
            .finish()
    }
}
