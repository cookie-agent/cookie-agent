use std::{collections::BTreeMap, path::Path};

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, CatalogRevision, ProviderId, ProviderStateRevision,
    ProviderStoreRevision, RuntimeRevision, SafeCode, SetupFieldId,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    BoundedSetupString, SafeSetupValue, Sha256Digest,
    secure_store::{SecureDirectory, SecureDirectoryLock},
};

use super::{
    PROVIDER_STORE_FILE, PROVIDER_STORE_LOCK_FILE, PROVIDER_STORE_SCHEMA_VERSION,
    types::{
        ClientConnectId, ClientRequestId, ConnectMutation, ConnectReceipt, DisconnectMutation,
        DisconnectReceipt, DurableProviderReceipt, MAX_PROVIDERS, MAX_RECEIPTS, ProviderAuthValues,
        ProviderConnectionGeneration, ProviderStoreError, ProviderStoreGeneration,
        ProviderStoreMutation, ProviderStoreSnapshot, SecretValue, StoredManagedConnection,
        StoredProviderPolicyProjection, validate_setup_values,
    },
};

const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;
const LEGACY_STORE_FILE: &str = "store-v1.json";
const PREVIOUS_STORE_FILE: &str = "store-v2.json";
const UNVERSIONED_STORE_FILE: &str = "store.json";

/// Secure provider-store handle rooted at one private directory.
#[derive(Debug)]
pub struct ProviderStore {
    directory: SecureDirectory,
}

impl ProviderStore {
    /// Opens the approved fixed per-user path.
    pub fn standard() -> Result<Self, ProviderStoreError> {
        Ok(Self {
            directory: SecureDirectory::user_data("providers")?,
        })
    }

    /// Opens or creates an absolute private provider-store directory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProviderStoreError> {
        Ok(Self {
            directory: SecureDirectory::open(path)?,
        })
    }

    /// Opens a private provider-store directory below a trusted anchor.
    pub fn open_in(
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, ProviderStoreError> {
        Ok(Self {
            directory: SecureDirectory::open_in(anchor, relative)?,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    /// Locks and rereads the complete store.
    pub fn load(&self) -> Result<ProviderStoreSnapshot, ProviderStoreError> {
        Ok(self.begin_transaction()?.state.snapshot())
    }

    /// Locks and rereads, returning state only when another process changed generation.
    pub fn reload_if_changed(
        &self,
        known: ProviderStoreGeneration,
    ) -> Result<Option<ProviderStoreSnapshot>, ProviderStoreError> {
        let transaction = self.begin_transaction()?;
        if transaction.state.generation == known {
            Ok(None)
        } else {
            Ok(Some(transaction.state.snapshot()))
        }
    }

    /// Starts a lock+reread transaction. The lock remains held through proposal compilation.
    pub fn begin_transaction(&self) -> Result<ProviderStoreTransaction<'_>, ProviderStoreError> {
        let lock = self.directory.lock(PROVIDER_STORE_LOCK_FILE)?;
        reject_obsolete_files(&lock)?;
        let state = match lock.read(PROVIDER_STORE_FILE, MAX_STORE_BYTES)? {
            Some(bytes) => {
                let bytes = Zeroizing::new(bytes);
                decode_state(bytes.as_ref())?
            }
            None => StoreState::fresh()?,
        };
        Ok(ProviderStoreTransaction {
            lock,
            state,
            transaction_id: Uuid::now_v7(),
        })
    }
}

/// Held provider-store transaction used to propose, compile, then commit exactly one state.
pub struct ProviderStoreTransaction<'a> {
    lock: SecureDirectoryLock<'a>,
    state: StoreState,
    transaction_id: Uuid,
}

impl std::fmt::Debug for ProviderStoreTransaction<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderStoreTransaction")
            .field("generation", &self.state.generation)
            .field("store_revision", &self.state.store_revision)
            .finish_non_exhaustive()
    }
}

impl ProviderStoreTransaction<'_> {
    #[must_use]
    pub fn snapshot(&self) -> ProviderStoreSnapshot {
        self.state.snapshot()
    }

    /// Validates replay/conflict/revisions and allocates the final connect proposal.
    pub fn propose_connect(
        &self,
        request: &ConnectMutation,
        current_catalog_revision: &CatalogRevision,
    ) -> Result<ConnectProposal, ProviderStoreError> {
        validate_managed_provider(&request.provider_id)?;
        validate_setup_values(&request.setup_values)?;
        request.policy.validate()?;

        let payload_digest = connect_payload_digest(request)?;

        if let Some(stored) = self.state.connect_receipts.get(&request.client_connect_id) {
            if !digest_equal(&stored.payload_digest, &payload_digest) {
                return Err(ProviderStoreError::IdempotencyConflict);
            }
            return Ok(ConnectProposal::Replay(Box::new(
                ProviderStoreMutation::Connect {
                    durable_receipt: stored.result.durable_receipt.clone(),
                    durable_connection: stored.result.durable_connection.clone(),
                },
            )));
        }

        if &request.expected_catalog_revision != current_catalog_revision
            || &request.policy.catalog_revision != current_catalog_revision
        {
            return Err(ProviderStoreError::CatalogRevisionConflict);
        }
        validate_expectation(&self.state, &request.expectation)?;

        let mut candidate = self.state.clone();
        candidate.generation = candidate.generation.checked_next()?;
        let committed_at = Timestamp::now();
        let connection_generation = self
            .state
            .providers
            .get(&request.provider_id)
            .map_or(Ok(ProviderConnectionGeneration::new(1)?), |connection| {
                connection.connection_generation.checked_next()
            })?;
        let setup_fingerprint = setup_fingerprint(&request.setup_values)?;
        let connection = StoredManagedConnection {
            provider_id: request.provider_id.clone(),
            setup_values: request.setup_values.clone(),
            setup_fingerprint,
            auth_method: request.auth_method.clone(),
            auth_values: request.auth_values.clone(),
            connection_generation,
            policy: request.policy.clone(),
            connected_at: committed_at,
        };
        let receipt = ConnectReceipt {
            durable_receipt: DurableProviderReceipt {
                receipt_id: Uuid::now_v7(),
                store_revision: placeholder_revision(),
                provider_state_revision: provider_state_revision(&placeholder_revision()),
                committed_at,
            },
            durable_connection: connection.descriptor(),
        };
        candidate
            .providers
            .insert(request.provider_id.clone(), connection);
        if candidate.providers.len() > MAX_PROVIDERS
            || candidate.connect_receipts.len() >= MAX_RECEIPTS
        {
            return Err(ProviderStoreError::InvalidRequest);
        }
        candidate.connect_receipts.insert(
            request.client_connect_id.clone(),
            StoredConnectReceipt {
                payload_digest,
                result: receipt.clone(),
            },
        );
        candidate.finalize_revision_for_connect(&request.client_connect_id)?;
        let receipt = candidate
            .connect_receipts
            .get(&request.client_connect_id)
            .expect("inserted connect receipt")
            .result
            .clone();
        let proposal = ProposedProviderStore::new(
            self.transaction_id,
            &self.state,
            candidate,
            ProviderStoreMutation::Connect {
                durable_receipt: receipt.durable_receipt,
                durable_connection: receipt.durable_connection,
            },
        )?;
        Ok(ConnectProposal::Proposed(Box::new(proposal)))
    }

    /// Validates replay/conflict/revisions/generation and allocates the final disconnect proposal.
    pub fn propose_disconnect(
        &self,
        request: &DisconnectMutation,
        current_runtime_revision: &RuntimeRevision,
    ) -> Result<DisconnectProposal, ProviderStoreError> {
        validate_managed_provider(&request.provider_id)?;
        let payload_digest = disconnect_payload_digest(request)?;
        if let Some(stored) = self
            .state
            .disconnect_receipts
            .get(&request.client_request_id)
        {
            if !digest_equal(&stored.payload_digest, &payload_digest) {
                return Err(ProviderStoreError::IdempotencyConflict);
            }
            return Ok(DisconnectProposal::Replay(Box::new(
                ProviderStoreMutation::Disconnect {
                    durable_receipt: stored.result.durable_receipt.clone(),
                    provider_id: stored.result.provider_id.clone(),
                    disconnected: stored.result.disconnected,
                },
            )));
        }

        if &request.expected_runtime_revision != current_runtime_revision {
            return Err(ProviderStoreError::RuntimeRevisionConflict);
        }
        if request.expected_store_generation != self.state.generation {
            return Err(ProviderStoreError::StoreGenerationConflict);
        }
        if request.expected_store_revision != self.state.store_revision {
            return Err(ProviderStoreError::StoreRevisionConflict);
        }
        if request.expected_provider_state_revision
            != provider_state_revision(&self.state.store_revision)
        {
            return Err(ProviderStoreError::ProviderStateRevisionConflict);
        }
        match (
            self.state.providers.get(&request.provider_id),
            request.expected_connection_generation,
        ) {
            (Some(connection), Some(expected)) if connection.connection_generation == expected => {}
            (None, None) => {}
            _ => return Err(ProviderStoreError::StaleConnectionGeneration),
        }

        let mut candidate = self.state.clone();
        candidate.generation = candidate.generation.checked_next()?;
        candidate.providers.remove(&request.provider_id);
        if candidate.disconnect_receipts.len() >= MAX_RECEIPTS {
            return Err(ProviderStoreError::InvalidRequest);
        }
        let committed_at = Timestamp::now();
        let result = DisconnectReceipt {
            durable_receipt: DurableProviderReceipt {
                receipt_id: Uuid::now_v7(),
                store_revision: placeholder_revision(),
                provider_state_revision: provider_state_revision(&placeholder_revision()),
                committed_at,
            },
            provider_id: request.provider_id.clone(),
            disconnected: true,
        };
        candidate.disconnect_receipts.insert(
            request.client_request_id.clone(),
            StoredDisconnectReceipt {
                payload_digest,
                result: result.clone(),
            },
        );
        candidate.finalize_revision_for_disconnect(&request.client_request_id)?;
        let result = candidate
            .disconnect_receipts
            .get(&request.client_request_id)
            .expect("inserted disconnect receipt")
            .result
            .clone();
        let proposal = ProposedProviderStore::new(
            self.transaction_id,
            &self.state,
            candidate,
            ProviderStoreMutation::Disconnect {
                durable_receipt: result.durable_receipt,
                provider_id: result.provider_id,
                disconnected: result.disconnected,
            },
        )?;
        Ok(DisconnectProposal::Proposed(Box::new(proposal)))
    }

    /// Atomically commits the exact pre-serialized proposal. No revision is allocated here.
    pub fn commit(
        self,
        proposal: ProposedProviderStore,
    ) -> Result<CommittedProviderStore, ProviderStoreError> {
        if proposal.transaction_id != self.transaction_id
            || proposal.base_generation != self.state.generation
            || proposal.base_revision != self.state.store_revision
        {
            return Err(ProviderStoreError::ProposalMismatch);
        }
        // Allocate the complete safe return value before the durable write. After replacement,
        // commit only moves prebuilt data so manager publication remains infallible.
        let snapshot = proposal.state.snapshot();
        let mutation = proposal.mutation.clone();
        self.lock
            .atomic_replace(PROVIDER_STORE_FILE, proposal.bytes.as_ref())?;
        Ok(CommittedProviderStore { snapshot, mutation })
    }
}

/// Connect proposal outcome. Replays require neither compilation nor publication.
#[derive(Debug)]
pub enum ConnectProposal {
    Replay(Box<ProviderStoreMutation>),
    Proposed(Box<ProposedProviderStore>),
}

/// Disconnect proposal outcome. Replays require neither compilation nor publication.
#[derive(Debug)]
pub enum DisconnectProposal {
    Replay(Box<ProviderStoreMutation>),
    Proposed(Box<ProposedProviderStore>),
}

/// Final proposed state and bytes compiled by P6 before commit.
pub struct ProposedProviderStore {
    transaction_id: Uuid,
    base_generation: ProviderStoreGeneration,
    base_revision: ProviderStoreRevision,
    state: StoreState,
    bytes: Zeroizing<Vec<u8>>,
    mutation: ProviderStoreMutation,
}

impl ProposedProviderStore {
    fn new(
        transaction_id: Uuid,
        base: &StoreState,
        state: StoreState,
        mutation: ProviderStoreMutation,
    ) -> Result<Self, ProviderStoreError> {
        state.validate()?;
        let bytes = Zeroizing::new(encode_state(&state)?);
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err(ProviderStoreError::InvalidStore);
        }
        Ok(Self {
            transaction_id,
            base_generation: base.generation,
            base_revision: base.store_revision.clone(),
            state,
            bytes,
            mutation,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> ProviderStoreSnapshot {
        self.state.snapshot()
    }

    #[must_use]
    pub fn mutation(&self) -> &ProviderStoreMutation {
        &self.mutation
    }
}

impl std::fmt::Debug for ProposedProviderStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProposedProviderStore")
            .field("base_generation", &self.base_generation)
            .field("base_revision", &self.base_revision)
            .field("generation", &self.state.generation)
            .field("store_revision", &self.state.store_revision)
            .field("mutation", &self.mutation)
            .finish()
    }
}

/// Durable result of committing a proposal.
#[derive(Debug)]
pub struct CommittedProviderStore {
    pub snapshot: ProviderStoreSnapshot,
    pub mutation: ProviderStoreMutation,
}

#[derive(Clone)]
struct StoreState {
    generation: ProviderStoreGeneration,
    store_revision: ProviderStoreRevision,
    providers: BTreeMap<ProviderId, StoredManagedConnection>,
    connect_receipts: BTreeMap<ClientConnectId, StoredConnectReceipt>,
    disconnect_receipts: BTreeMap<ClientRequestId, StoredDisconnectReceipt>,
}

impl StoreState {
    fn fresh() -> Result<Self, ProviderStoreError> {
        let mut state = Self {
            generation: ProviderStoreGeneration::new(1).expect("valid initial generation"),
            store_revision: placeholder_revision(),
            providers: BTreeMap::new(),
            connect_receipts: BTreeMap::new(),
            disconnect_receipts: BTreeMap::new(),
        };
        state.store_revision = state_revision(&state)?;
        Ok(state)
    }

    fn finalize_revision_for_connect(
        &mut self,
        id: &ClientConnectId,
    ) -> Result<(), ProviderStoreError> {
        let revision = state_revision(self)?;
        self.store_revision = revision.clone();
        let receipt = self
            .connect_receipts
            .get_mut(id)
            .ok_or(ProviderStoreError::InvalidStore)?;
        receipt.result.durable_receipt.store_revision = revision.clone();
        receipt.result.durable_receipt.provider_state_revision = provider_state_revision(&revision);
        Ok(())
    }

    fn finalize_revision_for_disconnect(
        &mut self,
        id: &ClientRequestId,
    ) -> Result<(), ProviderStoreError> {
        let revision = state_revision(self)?;
        self.store_revision = revision.clone();
        let receipt = self
            .disconnect_receipts
            .get_mut(id)
            .ok_or(ProviderStoreError::InvalidStore)?;
        receipt.result.durable_receipt.store_revision = revision.clone();
        receipt.result.durable_receipt.provider_state_revision = provider_state_revision(&revision);
        Ok(())
    }

    fn snapshot(&self) -> ProviderStoreSnapshot {
        ProviderStoreSnapshot {
            generation: self.generation,
            store_revision: self.store_revision.clone(),
            providers: self.providers.clone(),
            connect_receipts: self
                .connect_receipts
                .iter()
                .map(|(id, receipt)| (id.clone(), receipt.result.clone()))
                .collect(),
            disconnect_receipts: self
                .disconnect_receipts
                .iter()
                .map(|(id, receipt)| (id.clone(), receipt.result.clone()))
                .collect(),
        }
    }

    fn validate(&self) -> Result<(), ProviderStoreError> {
        if self.providers.len() > MAX_PROVIDERS
            || self.connect_receipts.len() > MAX_RECEIPTS
            || self.disconnect_receipts.len() > MAX_RECEIPTS
        {
            return Err(ProviderStoreError::InvalidStore);
        }
        if state_revision(self)? != self.store_revision {
            return Err(ProviderStoreError::InvalidStore);
        }
        for (provider_id, connection) in &self.providers {
            validate_managed_provider(provider_id).map_err(|_| ProviderStoreError::InvalidStore)?;
            if provider_id != &connection.provider_id {
                return Err(ProviderStoreError::InvalidStore);
            }
            validate_setup_values(&connection.setup_values)
                .map_err(|_| ProviderStoreError::InvalidStore)?;
            if setup_fingerprint(&connection.setup_values)? != connection.setup_fingerprint {
                return Err(ProviderStoreError::InvalidStore);
            }
            connection.policy.validate()?;
        }
        for receipt in self.connect_receipts.values() {
            validate_managed_provider(&receipt.result.durable_connection.provider_id)
                .map_err(|_| ProviderStoreError::InvalidStore)?;
            validate_setup_values(&receipt.result.durable_connection.setup_values)
                .map_err(|_| ProviderStoreError::InvalidStore)?;
            if receipt.result.durable_connection.setup_fingerprint
                != setup_fingerprint(&receipt.result.durable_connection.setup_values)?
                || receipt.result.durable_connection.credential_fields.len()
                    > super::types::MAX_AUTH_FIELDS
                || receipt
                    .result
                    .durable_connection
                    .credential_fields
                    .windows(2)
                    .any(|pair| pair[0] >= pair[1])
                || receipt.result.durable_receipt.provider_state_revision
                    != provider_state_revision(&receipt.result.durable_receipt.store_revision)
            {
                return Err(ProviderStoreError::InvalidStore);
            }
        }
        for receipt in self.disconnect_receipts.values() {
            validate_managed_provider(&receipt.result.provider_id)
                .map_err(|_| ProviderStoreError::InvalidStore)?;
            if !receipt.result.disconnected
                || receipt.result.durable_receipt.provider_state_revision
                    != provider_state_revision(&receipt.result.durable_receipt.store_revision)
            {
                return Err(ProviderStoreError::InvalidStore);
            }
        }
        if (!self.connect_receipts.is_empty() || !self.disconnect_receipts.is_empty())
            && !self
                .connect_receipts
                .values()
                .map(|receipt| &receipt.result.durable_receipt.store_revision)
                .chain(
                    self.disconnect_receipts
                        .values()
                        .map(|receipt| &receipt.result.durable_receipt.store_revision),
                )
                .any(|revision| revision == &self.store_revision)
        {
            return Err(ProviderStoreError::InvalidStore);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct StoredConnectReceipt {
    payload_digest: Sha256Digest,
    result: ConnectReceipt,
}

#[derive(Clone)]
struct StoredDisconnectReceipt {
    payload_digest: Sha256Digest,
    result: DisconnectReceipt,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ConnectPayload<'a> {
    client_connect_id: &'a ClientConnectId,
    provider_id: &'a ProviderId,
    expected_catalog_revision: &'a CatalogRevision,
    setup_values: &'a BTreeMap<SetupFieldId, SafeSetupValue>,
    auth_method: &'a AuthMethodId,
    auth_values: SecretMapRef<'a>,
    policy: &'a StoredProviderPolicyProjection,
}

#[derive(Clone, Copy)]
struct SecretMapRef<'a>(&'a ProviderAuthValues);

impl Serialize for SecretMapRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;

        let mut map = serializer.serialize_map(Some(self.0.0.len()))?;
        for (field, value) in &self.0.0 {
            map.serialize_entry(field, value.expose())?;
        }
        map.end()
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct DisconnectPayload<'a> {
    client_request_id: &'a ClientRequestId,
    provider_id: &'a ProviderId,
    expected_runtime_revision: &'a RuntimeRevision,
    expected_provider_state_revision: &'a ProviderStateRevision,
    expected_connection_generation: Option<ProviderConnectionGeneration>,
}

fn connect_payload_digest(request: &ConnectMutation) -> Result<Sha256Digest, ProviderStoreError> {
    sha256_jcs(&ConnectPayload {
        client_connect_id: &request.client_connect_id,
        provider_id: &request.provider_id,
        expected_catalog_revision: &request.expected_catalog_revision,
        setup_values: &request.setup_values,
        auth_method: &request.auth_method,
        auth_values: SecretMapRef(&request.auth_values),
        policy: &request.policy,
    })
}

fn disconnect_payload_digest(
    request: &DisconnectMutation,
) -> Result<Sha256Digest, ProviderStoreError> {
    sha256_jcs(&DisconnectPayload {
        client_request_id: &request.client_request_id,
        provider_id: &request.provider_id,
        expected_runtime_revision: &request.expected_runtime_revision,
        expected_provider_state_revision: &request.expected_provider_state_revision,
        expected_connection_generation: request.expected_connection_generation,
    })
}

fn reject_obsolete_files(lock: &SecureDirectoryLock<'_>) -> Result<(), ProviderStoreError> {
    if lock.read(LEGACY_STORE_FILE, MAX_STORE_BYTES)?.is_some()
        || lock.read(PREVIOUS_STORE_FILE, MAX_STORE_BYTES)?.is_some()
    {
        return Err(ProviderStoreError::LegacyStoreVersion);
    }
    if lock
        .read(UNVERSIONED_STORE_FILE, MAX_STORE_BYTES)?
        .is_some()
    {
        return Err(ProviderStoreError::UnversionedStore);
    }
    Ok(())
}

fn validate_managed_provider(provider_id: &ProviderId) -> Result<(), ProviderStoreError> {
    if provider_id.as_str().starts_with("custom.") {
        Err(ProviderStoreError::CustomProviderForbidden)
    } else {
        Ok(())
    }
}

fn validate_expectation(
    state: &StoreState,
    expectation: &super::types::ProviderStoreExpectation,
) -> Result<(), ProviderStoreError> {
    if expectation.generation != state.generation {
        return Err(ProviderStoreError::StoreGenerationConflict);
    }
    if expectation.store_revision != state.store_revision {
        return Err(ProviderStoreError::StoreRevisionConflict);
    }
    if expectation.provider_state_revision != provider_state_revision(&state.store_revision) {
        return Err(ProviderStoreError::ProviderStateRevisionConflict);
    }
    Ok(())
}

pub(crate) fn setup_fingerprint(
    setup: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<Sha256Digest, ProviderStoreError> {
    sha256_jcs(&disk_setup_values(setup))
}

fn sha256_jcs(value: &impl Serialize) -> Result<Sha256Digest, ProviderStoreError> {
    let bytes = jcs_bytes(value)?;
    let mut hasher = Sha256::new();
    hasher.update(&*bytes);
    Sha256Digest::new(format!("{:x}", hasher.finalize())).map_err(|_| ProviderStoreError::Encoding)
}

fn digest_equal(left: &Sha256Digest, right: &Sha256Digest) -> bool {
    left.as_str()
        .bytes()
        .zip(right.as_str().bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn placeholder_revision() -> ProviderStoreRevision {
    let digest = Sha256::digest([]);
    ProviderStoreRevision::new(format!("sha256:{digest:x}")).expect("SHA-256 is a valid revision")
}

fn state_revision(state: &StoreState) -> Result<ProviderStoreRevision, ProviderStoreError> {
    let digest = sha256_jcs(&RevisionState::from(state))?;
    ProviderStoreRevision::new(format!("sha256:{}", digest.as_str()))
        .map_err(|_| ProviderStoreError::Encoding)
}

fn provider_state_revision(revision: &ProviderStoreRevision) -> ProviderStateRevision {
    ProviderStateRevision::new(revision.as_str().to_owned())
        .expect("provider-store revision is a provider-state revision")
}

// Exact durable-state projection used for the revision. Only the self-referential top-level and
// receipt revision fields are omitted; every other persisted field participates in the JCS hash.
#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionState {
    schema_version: u32,
    generation: ProviderStoreGeneration,
    providers: BTreeMap<ProviderId, DiskConnection>,
    connect_receipts: BTreeMap<ClientConnectId, RevisionConnectReceipt>,
    disconnect_receipts: BTreeMap<ClientRequestId, RevisionDisconnectReceipt>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionConnectReceipt {
    payload_digest: Sha256Digest,
    result: RevisionConnectResult,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionConnectResult {
    durable_receipt: RevisionDurableReceipt,
    durable_connection: DiskConnectionDescriptor,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionDisconnectReceipt {
    payload_digest: Sha256Digest,
    result: RevisionDisconnectResult,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionDisconnectResult {
    durable_receipt: RevisionDurableReceipt,
    provider_id: ProviderId,
    disconnected: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct RevisionDurableReceipt {
    receipt_id: Uuid,
    committed_at: Timestamp,
}

impl From<&StoreState> for RevisionState {
    fn from(state: &StoreState) -> Self {
        Self {
            schema_version: PROVIDER_STORE_SCHEMA_VERSION,
            generation: state.generation,
            providers: state
                .providers
                .iter()
                .map(|(id, connection)| (id.clone(), DiskConnection::from(connection)))
                .collect(),
            connect_receipts: state
                .connect_receipts
                .iter()
                .map(|(id, receipt)| {
                    (
                        id.clone(),
                        RevisionConnectReceipt {
                            payload_digest: receipt.payload_digest.clone(),
                            result: RevisionConnectResult {
                                durable_receipt: RevisionDurableReceipt::from(
                                    &receipt.result.durable_receipt,
                                ),
                                durable_connection: DiskConnectionDescriptor::from(
                                    &receipt.result.durable_connection,
                                ),
                            },
                        },
                    )
                })
                .collect(),
            disconnect_receipts: state
                .disconnect_receipts
                .iter()
                .map(|(id, receipt)| {
                    (
                        id.clone(),
                        RevisionDisconnectReceipt {
                            payload_digest: receipt.payload_digest.clone(),
                            result: RevisionDisconnectResult {
                                durable_receipt: RevisionDurableReceipt::from(
                                    &receipt.result.durable_receipt,
                                ),
                                provider_id: receipt.result.provider_id.clone(),
                                disconnected: receipt.result.disconnected,
                            },
                        },
                    )
                })
                .collect(),
        }
    }
}

impl From<&DurableProviderReceipt> for RevisionDurableReceipt {
    fn from(receipt: &DurableProviderReceipt) -> Self {
        Self {
            receipt_id: receipt.receipt_id,
            committed_at: receipt.committed_at,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskState {
    schema_version: u32,
    generation: ProviderStoreGeneration,
    store_revision: ProviderStoreRevision,
    providers: BTreeMap<ProviderId, DiskConnection>,
    connect_receipts: BTreeMap<ClientConnectId, DiskConnectReceipt>,
    disconnect_receipts: BTreeMap<ClientRequestId, DiskDisconnectReceipt>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskConnection {
    provider_id: ProviderId,
    setup_values: BTreeMap<SetupFieldId, DiskSetupValue>,
    setup_fingerprint: Sha256Digest,
    auth_method: AuthMethodId,
    auth_values: BTreeMap<AuthFieldName, DiskSecret>,
    connection_generation: ProviderConnectionGeneration,
    policy: StoredProviderPolicyProjection,
    connected_at: Timestamp,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskConnectReceipt {
    payload_digest: Sha256Digest,
    result: DiskConnectResult,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskConnectResult {
    durable_receipt: DiskDurableReceipt,
    durable_connection: DiskConnectionDescriptor,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskDisconnectReceipt {
    payload_digest: Sha256Digest,
    result: DiskDisconnectResult,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskDisconnectResult {
    durable_receipt: DiskDurableReceipt,
    provider_id: ProviderId,
    disconnected: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskDurableReceipt {
    receipt_id: Uuid,
    store_revision: ProviderStoreRevision,
    provider_state_revision: ProviderStateRevision,
    committed_at: Timestamp,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskConnectionDescriptor {
    provider_id: ProviderId,
    setup_values: BTreeMap<SetupFieldId, DiskSetupValue>,
    setup_fingerprint: Sha256Digest,
    recipe_fingerprint: Sha256Digest,
    auth_method: AuthMethodId,
    credential_fields: Vec<AuthFieldName>,
    connection_generation: ProviderConnectionGeneration,
    connected_at: Timestamp,
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct DiskSecret(String);

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum DiskSetupValue {
    String(BoundedSetupString),
    Code(SafeCode),
    Integer(i64),
    Bool(bool),
}

impl From<&SafeSetupValue> for DiskSetupValue {
    fn from(value: &SafeSetupValue) -> Self {
        match value {
            SafeSetupValue::String(value) => Self::String(value.clone()),
            SafeSetupValue::Code(value) => Self::Code(value.clone()),
            SafeSetupValue::Integer(value) => Self::Integer(*value),
            SafeSetupValue::Bool(value) => Self::Bool(*value),
        }
    }
}

impl From<DiskSetupValue> for SafeSetupValue {
    fn from(value: DiskSetupValue) -> Self {
        match value {
            DiskSetupValue::String(value) => Self::String(value),
            DiskSetupValue::Code(value) => Self::Code(value),
            DiskSetupValue::Integer(value) => Self::Integer(value),
            DiskSetupValue::Bool(value) => Self::Bool(value),
        }
    }
}

fn disk_setup_values(
    values: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> BTreeMap<SetupFieldId, DiskSetupValue> {
    values
        .iter()
        .map(|(field, value)| (field.clone(), DiskSetupValue::from(value)))
        .collect()
}

fn live_setup_values(
    values: BTreeMap<SetupFieldId, DiskSetupValue>,
) -> BTreeMap<SetupFieldId, SafeSetupValue> {
    values
        .into_iter()
        .map(|(field, value)| (field, SafeSetupValue::from(value)))
        .collect()
}

impl DiskSecret {
    fn into_secret(mut self) -> Result<SecretValue, ProviderStoreError> {
        SecretValue::new(std::mem::take(&mut self.0)).map_err(|_| ProviderStoreError::InvalidStore)
    }
}

impl Drop for DiskSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<&SecretValue> for DiskSecret {
    fn from(value: &SecretValue) -> Self {
        Self(value.expose().to_owned())
    }
}

fn encode_state(state: &StoreState) -> Result<Vec<u8>, ProviderStoreError> {
    let disk = DiskState::from(state);
    let mut bytes = jcs_bytes(&disk)?;
    Ok(std::mem::take(&mut *bytes))
}

fn jcs_bytes(value: &impl Serialize) -> Result<Zeroizing<Vec<u8>>, ProviderStoreError> {
    let value = StrictValue(serde_json::to_value(value).map_err(|_| ProviderStoreError::Encoding)?);
    let mut bytes = Zeroizing::new(Vec::new());
    write_jcs(&value.0, &mut bytes)?;
    Ok(bytes)
}

fn write_jcs(value: &Value, output: &mut Vec<u8>) -> Result<(), ProviderStoreError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            if number.as_i64().is_none() && number.as_u64().is_none() {
                return Err(ProviderStoreError::Encoding);
            }
            output.extend_from_slice(number.to_string().as_bytes());
        }
        Value::String(value) => write_jcs_string(value, output)?,
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_jcs(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_jcs_string(key, output)?;
                output.push(b':');
                write_jcs(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn write_jcs_string(value: &str, output: &mut Vec<u8>) -> Result<(), ProviderStoreError> {
    let encoded =
        Zeroizing::new(serde_json::to_string(value).map_err(|_| ProviderStoreError::Encoding)?);
    output.extend_from_slice(encoded.as_bytes());
    Ok(())
}

fn decode_state(bytes: &[u8]) -> Result<StoreState, ProviderStoreError> {
    let strict: StrictValue =
        serde_json::from_slice(bytes).map_err(|_| ProviderStoreError::InvalidStore)?;
    let object = strict
        .0
        .as_object()
        .ok_or(ProviderStoreError::InvalidStore)?;
    match object.get("schema_version") {
        None => return Err(ProviderStoreError::UnversionedStore),
        Some(Value::Number(number))
            if number.as_u64().is_some_and(|version| {
                version > 0 && version < u64::from(PROVIDER_STORE_SCHEMA_VERSION)
            }) =>
        {
            return Err(ProviderStoreError::LegacyStoreVersion);
        }
        Some(Value::Number(number))
            if number.as_u64() == Some(u64::from(PROVIDER_STORE_SCHEMA_VERSION)) => {}
        Some(_) => return Err(ProviderStoreError::UnsupportedStoreVersion),
    }
    let disk: DiskState = serde_json::from_value(strict.into_value())
        .map_err(|_| ProviderStoreError::InvalidStore)?;
    let state = StoreState::try_from(disk)?;
    state.validate()?;
    Ok(state)
}

struct StrictValue(Value);

impl StrictValue {
    fn into_value(mut self) -> Value {
        std::mem::take(&mut self.0)
    }
}

impl Drop for StrictValue {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.0);
    }
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => {
            for value in values {
                zeroize_json_value(value);
            }
        }
        Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                zeroize_json_value(&mut value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("strict JSON")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(Value::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Value::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Value::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Value::Null)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictValue>()? {
                    values.push(value.into_value());
                }
                Ok(Value::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                    if values.insert(key, value.into_value()).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(Value::Object(values))
            }
        }
        deserializer.deserialize_any(Visitor).map(Self)
    }
}

impl From<&StoreState> for DiskState {
    fn from(state: &StoreState) -> Self {
        Self {
            schema_version: PROVIDER_STORE_SCHEMA_VERSION,
            generation: state.generation,
            store_revision: state.store_revision.clone(),
            providers: state
                .providers
                .iter()
                .map(|(id, connection)| (id.clone(), DiskConnection::from(connection)))
                .collect(),
            connect_receipts: state
                .connect_receipts
                .iter()
                .map(|(id, receipt)| (id.clone(), DiskConnectReceipt::from(receipt)))
                .collect(),
            disconnect_receipts: state
                .disconnect_receipts
                .iter()
                .map(|(id, receipt)| (id.clone(), DiskDisconnectReceipt::from(receipt)))
                .collect(),
        }
    }
}

impl TryFrom<DiskState> for StoreState {
    type Error = ProviderStoreError;

    fn try_from(state: DiskState) -> Result<Self, Self::Error> {
        if state.schema_version != PROVIDER_STORE_SCHEMA_VERSION {
            return Err(ProviderStoreError::UnsupportedStoreVersion);
        }
        Ok(Self {
            generation: state.generation,
            store_revision: state.store_revision,
            providers: state
                .providers
                .into_iter()
                .map(|(id, connection)| Ok((id, StoredManagedConnection::try_from(connection)?)))
                .collect::<Result<_, ProviderStoreError>>()?,
            connect_receipts: state
                .connect_receipts
                .into_iter()
                .map(|(id, receipt)| Ok((id, StoredConnectReceipt::try_from(receipt)?)))
                .collect::<Result<_, ProviderStoreError>>()?,
            disconnect_receipts: state
                .disconnect_receipts
                .into_iter()
                .map(|(id, receipt)| Ok((id, StoredDisconnectReceipt::from(receipt))))
                .collect::<Result<_, ProviderStoreError>>()?,
        })
    }
}

impl From<&StoredManagedConnection> for DiskConnection {
    fn from(connection: &StoredManagedConnection) -> Self {
        Self {
            provider_id: connection.provider_id.clone(),
            setup_values: disk_setup_values(&connection.setup_values),
            setup_fingerprint: connection.setup_fingerprint.clone(),
            auth_method: connection.auth_method.clone(),
            auth_values: connection
                .auth_values
                .0
                .iter()
                .map(|(field, value)| (field.clone(), DiskSecret::from(value)))
                .collect(),
            connection_generation: connection.connection_generation,
            policy: connection.policy.clone(),
            connected_at: connection.connected_at,
        }
    }
}

impl TryFrom<DiskConnection> for StoredManagedConnection {
    type Error = ProviderStoreError;

    fn try_from(connection: DiskConnection) -> Result<Self, Self::Error> {
        if connection.auth_values.len() > super::types::MAX_AUTH_FIELDS {
            return Err(ProviderStoreError::InvalidStore);
        }
        Ok(Self {
            provider_id: connection.provider_id,
            setup_values: live_setup_values(connection.setup_values),
            setup_fingerprint: connection.setup_fingerprint,
            auth_method: connection.auth_method,
            auth_values: ProviderAuthValues(
                connection
                    .auth_values
                    .into_iter()
                    .map(|(field, value)| Ok((field, value.into_secret()?)))
                    .collect::<Result<_, ProviderStoreError>>()?,
            ),
            connection_generation: connection.connection_generation,
            policy: connection.policy,
            connected_at: connection.connected_at,
        })
    }
}

impl From<&StoredConnectReceipt> for DiskConnectReceipt {
    fn from(receipt: &StoredConnectReceipt) -> Self {
        Self {
            payload_digest: receipt.payload_digest.clone(),
            result: DiskConnectResult {
                durable_receipt: DiskDurableReceipt::from(&receipt.result.durable_receipt),
                durable_connection: DiskConnectionDescriptor::from(
                    &receipt.result.durable_connection,
                ),
            },
        }
    }
}

impl TryFrom<DiskConnectReceipt> for StoredConnectReceipt {
    type Error = ProviderStoreError;

    fn try_from(receipt: DiskConnectReceipt) -> Result<Self, Self::Error> {
        Ok(Self {
            payload_digest: receipt.payload_digest,
            result: ConnectReceipt {
                durable_receipt: DurableProviderReceipt::from(receipt.result.durable_receipt),
                durable_connection: super::types::DurableConnectionDescriptor::from(
                    receipt.result.durable_connection,
                ),
            },
        })
    }
}

impl From<&StoredDisconnectReceipt> for DiskDisconnectReceipt {
    fn from(receipt: &StoredDisconnectReceipt) -> Self {
        Self {
            payload_digest: receipt.payload_digest.clone(),
            result: DiskDisconnectResult {
                durable_receipt: DiskDurableReceipt::from(&receipt.result.durable_receipt),
                provider_id: receipt.result.provider_id.clone(),
                disconnected: receipt.result.disconnected,
            },
        }
    }
}

impl From<DiskDisconnectReceipt> for StoredDisconnectReceipt {
    fn from(receipt: DiskDisconnectReceipt) -> Self {
        Self {
            payload_digest: receipt.payload_digest,
            result: DisconnectReceipt {
                durable_receipt: DurableProviderReceipt::from(receipt.result.durable_receipt),
                provider_id: receipt.result.provider_id,
                disconnected: receipt.result.disconnected,
            },
        }
    }
}

impl From<&DurableProviderReceipt> for DiskDurableReceipt {
    fn from(receipt: &DurableProviderReceipt) -> Self {
        Self {
            receipt_id: receipt.receipt_id,
            store_revision: receipt.store_revision.clone(),
            provider_state_revision: receipt.provider_state_revision.clone(),
            committed_at: receipt.committed_at,
        }
    }
}

impl From<DiskDurableReceipt> for DurableProviderReceipt {
    fn from(receipt: DiskDurableReceipt) -> Self {
        Self {
            receipt_id: receipt.receipt_id,
            store_revision: receipt.store_revision,
            provider_state_revision: receipt.provider_state_revision,
            committed_at: receipt.committed_at,
        }
    }
}

impl From<&super::types::DurableConnectionDescriptor> for DiskConnectionDescriptor {
    fn from(connection: &super::types::DurableConnectionDescriptor) -> Self {
        Self {
            provider_id: connection.provider_id.clone(),
            setup_values: disk_setup_values(&connection.setup_values),
            setup_fingerprint: connection.setup_fingerprint.clone(),
            recipe_fingerprint: connection.recipe_fingerprint.clone(),
            auth_method: connection.auth_method.clone(),
            credential_fields: connection.credential_fields.clone(),
            connection_generation: connection.connection_generation,
            connected_at: connection.connected_at,
        }
    }
}

impl From<DiskConnectionDescriptor> for super::types::DurableConnectionDescriptor {
    fn from(connection: DiskConnectionDescriptor) -> Self {
        Self {
            provider_id: connection.provider_id,
            setup_values: live_setup_values(connection.setup_values),
            setup_fingerprint: connection.setup_fingerprint,
            recipe_fingerprint: connection.recipe_fingerprint,
            auth_method: connection.auth_method,
            credential_fields: connection.credential_fields,
            connection_generation: connection.connection_generation,
            connected_at: connection.connected_at,
        }
    }
}
