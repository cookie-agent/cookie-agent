//! The models crate's integration tests, one module per surface, in one
//! test binary; `release_integrity` and the manual `compile_timing` stay
//! binaries of their own.

mod adapter_requests;
mod dynamic_catalog;
mod family_compiler;
mod family_registry;
mod manager_runtime;
mod manifest_v1;
mod model_configuration;
mod model_set;
mod model_type_serde_equivalence;
mod provider_store_v3;
mod reasoning_requests;
mod secure_store;
mod secure_store_windows;
mod updater;
mod wire_snapshots;
