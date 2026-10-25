//! Web Push message encryption (RFC 8291, `aes128gcm` of RFC 8188) and the
//! daemon's VAPID identity (RFC 8292), on `aws-lc-rs`, the crypto library
//! rustls already brings (push notifications spec section 6).

use aws_lc_rs::{
    aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    agreement::{self, ECDH_P256, PrivateKey, UnparsedPublicKey},
    hkdf::{HKDF_SHA256, KeyType, Salt},
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

/// The record size the header announces. One record carries the whole
/// message, which push services cap at 4096 octets.
const RECORD_SIZE: u32 = 4096;

/// The plaintext a message can carry: a record of 4096 octets less the
/// 86-octet header, the 16-octet tag, and the padding delimiter.
pub(crate) const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 86 - 16 - 1;

/// What went wrong encrypting or signing.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PushCryptoError {
    #[error("the subscription's {0} key is not valid")]
    InvalidKey(&'static str),
    #[error("the message is longer than one push record")]
    TooLong,
    #[error("the VAPID key is not a P-256 PKCS#8 key")]
    InvalidVapidKey,
    #[error("a cryptographic operation failed")]
    Failed,
}

/// The length an HKDF expansion produces.
struct Length(usize);

impl KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

/// `HKDF-Expand(prk, info, length)` of `HKDF-Extract(salt, ikm)`.
fn hkdf(
    salt: &[u8],
    ikm: &[u8],
    info: &[&[u8]],
    length: usize,
) -> Result<Vec<u8>, PushCryptoError> {
    let prk = Salt::new(HKDF_SHA256, salt).extract(ikm);
    let okm = prk
        .expand(info, Length(length))
        .map_err(|_| PushCryptoError::Failed)?;
    let mut out = vec![0; length];
    okm.fill(&mut out).map_err(|_| PushCryptoError::Failed)?;
    Ok(out)
}

/// Decodes a base64url key a browser handed over, padded or not.
pub(crate) fn decode_key(value: &str, which: &'static str) -> Result<Vec<u8>, PushCryptoError> {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|_| PushCryptoError::InvalidKey(which))
}

/// Encrypts `plaintext` for the subscription with `ua_public` (its
/// `p256dh`, an uncompressed P-256 point) and `auth_secret` (its `auth`):
/// one `aes128gcm` record, header included, ready to POST.
pub(crate) fn encrypt(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PushCryptoError> {
    let private = PrivateKey::generate(&ECDH_P256).map_err(|_| PushCryptoError::Failed)?;
    let mut salt = [0; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| PushCryptoError::Failed)?;
    encrypt_with(&private, &salt, ua_public, auth_secret, plaintext)
}

/// [`encrypt`] with a given application server key and salt.
fn encrypt_with(
    as_private: &PrivateKey,
    salt: &[u8; 16],
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PushCryptoError> {
    if ua_public.len() != 65 || ua_public[0] != 4 {
        return Err(PushCryptoError::InvalidKey("p256dh"));
    }
    if auth_secret.len() != 16 {
        return Err(PushCryptoError::InvalidKey("auth"));
    }
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(PushCryptoError::TooLong);
    }
    let as_public = as_private
        .compute_public_key()
        .map_err(|_| PushCryptoError::Failed)?;
    let as_public = as_public.as_ref();
    let ecdh_secret = agreement::agree(
        as_private,
        UnparsedPublicKey::new(&ECDH_P256, ua_public),
        PushCryptoError::InvalidKey("p256dh"),
        |secret| Ok(secret.to_vec()),
    )?;
    let ikm = hkdf(
        auth_secret,
        &ecdh_secret,
        &[b"WebPush: info\0", ua_public, as_public],
        32,
    )?;
    let cek = hkdf(salt, &ikm, &[b"Content-Encoding: aes128gcm\0"], 16)?;
    let nonce = hkdf(salt, &ikm, &[b"Content-Encoding: nonce\0"], 12)?;
    let key =
        LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).map_err(|_| PushCryptoError::Failed)?);
    let nonce = Nonce::try_assume_unique_for_key(&nonce).map_err(|_| PushCryptoError::Failed)?;
    // The last (and only) record ends with the delimiter 0x02, no padding.
    let mut record = Vec::with_capacity(plaintext.len() + 17);
    record.extend_from_slice(plaintext);
    record.push(2);
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut record)
        .map_err(|_| PushCryptoError::Failed)?;
    let mut body = Vec::with_capacity(86 + record.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(u8::try_from(as_public.len()).map_err(|_| PushCryptoError::Failed)?);
    body.extend_from_slice(as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// The daemon's VAPID key pair: a P-256 signing key kept as PKCS#8.
pub(crate) struct VapidKey {
    pair: EcdsaKeyPair,
}

impl VapidKey {
    /// A new key and its PKCS#8 document, to store.
    pub(crate) fn generate() -> Result<(Self, Vec<u8>), PushCryptoError> {
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| PushCryptoError::Failed)?;
        let pkcs8 = document.as_ref().to_vec();
        Ok((Self::from_pkcs8(&pkcs8)?, pkcs8))
    }

    pub(crate) fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, PushCryptoError> {
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8)
            .map(|pair| Self { pair })
            .map_err(|_| PushCryptoError::InvalidVapidKey)
    }

    /// The public key, an uncompressed point in base64url: what browsers
    /// subscribe with (`applicationServerKey`) and the `k` of the header.
    pub(crate) fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref())
    }

    /// A VAPID JWT (ES256) for the push service at `audience` (an origin),
    /// expiring at `expires` (Unix seconds), naming `subject`.
    pub(crate) fn token(
        &self,
        audience: &str,
        expires: i64,
        subject: &str,
    ) -> Result<String, PushCryptoError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({ "aud": audience, "exp": expires, "sub": subject });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signed = format!("{header}.{claims}");
        let signature = self
            .pair
            .sign(&SystemRandom::new(), signed.as_bytes())
            .map_err(|_| PushCryptoError::Failed)?;
        Ok(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey as VerifyingKey};

    use super::*;

    fn b64(value: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD
            .decode(value.split_whitespace().collect::<String>())
            .expect("base64url")
    }

    /// RFC 8291 section 5 and appendix A: fixed keys, salt, and plaintext
    /// encrypt to the published message.
    #[test]
    fn the_rfc_8291_example_encrypts_to_its_published_message() {
        let as_private = PrivateKey::from_private_key(
            &ECDH_P256,
            &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        )
        .expect("as_private");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().expect("salt");
        let ua_public = b64(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let auth_secret = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let body = encrypt_with(
            &as_private,
            &salt,
            &ua_public,
            &auth_secret,
            b"When I grow up, I want to be a watermelon",
        )
        .expect("encrypts");
        assert_eq!(
            body,
            b64(
                "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
                 mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
                 pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
            )
        );
    }

    #[test]
    fn a_fresh_message_has_the_header_and_fits_a_record() {
        let receiver = PrivateKey::generate(&ECDH_P256).expect("receiver");
        let ua_public = receiver.compute_public_key().expect("public");
        let body = encrypt(ua_public.as_ref(), &[7; 16], &[b'x'; MAX_PLAINTEXT]).expect("fits");
        assert_eq!(body.len(), RECORD_SIZE as usize);
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(body[20], 65);
        assert!(matches!(
            encrypt(ua_public.as_ref(), &[7; 16], &[b'x'; MAX_PLAINTEXT + 1]),
            Err(PushCryptoError::TooLong)
        ));
        assert!(matches!(
            encrypt(&ua_public.as_ref()[1..], &[7; 16], b"x"),
            Err(PushCryptoError::InvalidKey("p256dh"))
        ));
        assert!(matches!(
            encrypt(ua_public.as_ref(), &[7; 15], b"x"),
            Err(PushCryptoError::InvalidKey("auth"))
        ));
    }

    /// The token verifies with the public key the daemon hands browsers,
    /// and carries `aud`, `exp`, and `sub` (RFC 8292 section 2).
    #[test]
    fn the_vapid_token_verifies_with_the_public_key() {
        let (key, pkcs8) = VapidKey::generate().expect("key");
        let reloaded = VapidKey::from_pkcs8(&pkcs8).expect("reloads");
        assert_eq!(reloaded.public_key(), key.public_key());
        let token = key
            .token(
                "https://web.push.apple.com",
                1_700_000_000,
                "https://example.com",
            )
            .expect("token");
        let mut parts = token.split('.');
        let (header, claims, signature) = (
            parts.next().expect("header"),
            parts.next().expect("claims"),
            parts.next().expect("signature"),
        );
        assert!(parts.next().is_none());
        let public = b64(&key.public_key());
        assert_eq!(public.len(), 65);
        VerifyingKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(format!("{header}.{claims}").as_bytes(), &b64(signature))
            .expect("verifies");
        let claims: serde_json::Value = serde_json::from_slice(&b64(claims)).expect("json");
        assert_eq!(
            claims,
            serde_json::json!({
                "aud": "https://web.push.apple.com",
                "exp": 1_700_000_000,
                "sub": "https://example.com",
            })
        );
        assert!(VapidKey::from_pkcs8(b"not a key").is_err());
    }
}
