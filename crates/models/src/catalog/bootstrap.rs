use sha2::{Digest as _, Sha256};

use super::CatalogError;

pub const MODELS_DEV_BOOTSTRAP_COMMIT: &str = "c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
pub const MODELS_DEV_BOOTSTRAP_BYTES: usize = 3_567_054;
pub const MODELS_DEV_BOOTSTRAP_SHA256: &str =
    "d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a";
pub const MODELS_DEV_BOOTSTRAP_SOURCE: &str =
    "https://github.com/anomalyco/models.dev@c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
pub const MODELS_DEV_BOOTSTRAP: &[u8] = include_bytes!("../../catalog/models-dev.json");

pub(crate) fn validated_bootstrap() -> Result<&'static [u8], CatalogError> {
    let digest = format!("{:x}", Sha256::digest(MODELS_DEV_BOOTSTRAP));
    if MODELS_DEV_BOOTSTRAP.len() == MODELS_DEV_BOOTSTRAP_BYTES
        && digest == MODELS_DEV_BOOTSTRAP_SHA256
    {
        Ok(MODELS_DEV_BOOTSTRAP)
    } else {
        Err(CatalogError::new(
            "bootstrap_integrity_failed",
            "bundled catalog bootstrap integrity check failed",
        ))
    }
}
