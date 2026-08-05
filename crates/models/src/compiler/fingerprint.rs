use serde::Serialize;

use crate::Sha256Digest;

pub(crate) fn fingerprint<T: Serialize>(domain: &str, value: &T) -> Sha256Digest {
    Sha256Digest::hash(domain, value).expect("safe compiler fingerprint serializes")
}
