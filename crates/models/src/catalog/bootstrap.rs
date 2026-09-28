pub const MODELS_DEV_BOOTSTRAP_COMMIT: &str = "c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
pub const MODELS_DEV_BOOTSTRAP_BYTES: usize = 3_567_054;
pub const MODELS_DEV_BOOTSTRAP_SHA256: &str =
    "d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a";
pub const MODELS_DEV_BOOTSTRAP_SOURCE: &str =
    "https://github.com/anomalyco/models.dev@c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
/// The bundled catalog. It is compiled into the binary, so its integrity
/// against the pinned size and digest is checked by a unit test rather than
/// hashed again at runtime; the parser still validates it structurally.
pub const MODELS_DEV_BOOTSTRAP: &[u8] = include_bytes!("../../catalog/models-dev.json");

#[cfg(test)]
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::*;

    #[test]
    fn bundled_bootstrap_matches_its_pinned_size_and_digest() {
        assert_eq!(MODELS_DEV_BOOTSTRAP.len(), MODELS_DEV_BOOTSTRAP_BYTES);
        assert_eq!(
            format!("{:x}", Sha256::digest(MODELS_DEV_BOOTSTRAP)),
            MODELS_DEV_BOOTSTRAP_SHA256
        );
    }
}
