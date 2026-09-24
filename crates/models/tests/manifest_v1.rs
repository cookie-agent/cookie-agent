use cookie_agent_identity::{
    CatalogRevision, ModelRevision, ProviderStateRevision, RecipeRegistryRevision,
};
use cookie_agent_models::manifests::{
    ModelSnapshotPayloadV1, build_manifest, canonical_payload_bytes,
};
use sha2::{Digest as _, Sha256};

fn revision<T, E: std::fmt::Debug>(
    label: &str,
    constructor: impl FnOnce(String) -> Result<T, E>,
) -> T {
    constructor(format!("sha256:{:x}", Sha256::digest(label.as_bytes()))).unwrap()
}

fn payload(catalog: &str) -> ModelSnapshotPayloadV1 {
    ModelSnapshotPayloadV1 {
        catalog_revision: revision(catalog, CatalogRevision::new),
        recipe_registry_revision: revision("recipes", RecipeRegistryRevision::new),
        provider_state_revision: revision("providers", ProviderStateRevision::new),
        model_revision: revision("models", ModelRevision::new),
        blueprints: Vec::new(),
    }
}

#[test]
fn manifest_revision_is_the_digest_of_the_canonical_payload() {
    let manifest = build_manifest(payload("catalog")).unwrap();
    let canonical = canonical_payload_bytes(&manifest.payload).unwrap();
    assert_eq!(
        manifest.revision.as_str(),
        format!("sha256:{:x}", Sha256::digest(&canonical))
    );
    assert_eq!(manifest.payload, payload("catalog"));
}

#[test]
fn equal_payloads_share_a_revision_and_different_ones_do_not() {
    let first = build_manifest(payload("catalog")).unwrap();
    let again = build_manifest(payload("catalog")).unwrap();
    let other = build_manifest(payload("other catalog")).unwrap();
    assert_eq!(first.revision, again.revision);
    assert_ne!(first.revision, other.revision);
}
