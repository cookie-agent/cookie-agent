//! Secure per-user managed provider store schema 3.

mod store;
mod types;

pub(crate) use store::setup_fingerprint;

pub use store::{
    CommittedProviderStore, ConnectProposal, DisconnectProposal, ProposedProviderStore,
    ProviderStore, ProviderStoreTransaction,
};
pub use types::{
    ClientConnectId, ClientRequestId, ConnectMutation, DisconnectMutation,
    DurableConnectionDescriptor, DurableProviderReceipt, ProviderAuthValues,
    ProviderConnectionGeneration, ProviderStoreError, ProviderStoreExpectation,
    ProviderStoreGeneration, ProviderStoreMutation, ProviderStoreSnapshot, SafePolicyString,
    SafePolicyValue, StoredManagedConnection, StoredModelOverrideProjection,
    StoredProviderPolicyProjection,
};

/// The only accepted provider-store schema.
pub const PROVIDER_STORE_SCHEMA_VERSION: u32 = 3;
/// Fixed provider-store body filename.
pub const PROVIDER_STORE_FILE: &str = "store-v3.json";
/// Fixed provider-store lock filename.
pub const PROVIDER_STORE_LOCK_FILE: &str = "store-v3.lock";
