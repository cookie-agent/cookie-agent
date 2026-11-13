//! The signature checks a webhook may add to its URL token
//! (`docs/site/specs/webhooks.md` section 6). Every comparison is constant
//! time.

use aws_lc_rs::{constant_time, hmac};
use base64::Engine as _;
use jiff::Timestamp;

use super::ids::{RandomError, random_secret_bytes};
use super::request::{DeliveryHeader, WebhookRequest};

/// How far a Standard Webhooks timestamp may be from now, either way.
pub const STANDARD_TOLERANCE_SECONDS: i64 = 5 * 60;
/// The prefix of a Standard Webhooks secret.
pub const STANDARD_SECRET_PREFIX: &str = "whsec_";

/// How a generic HMAC signature is written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureEncoding {
    /// Hexadecimal, in either case.
    Hex,
    /// Standard base64, padded or not.
    Base64,
}

/// One webhook's check, with its secret.
#[derive(Clone, Eq, PartialEq)]
pub enum Verifier {
    /// GitHub: `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>`.
    GitHub { secret: String },
    /// [Standard Webhooks](https://www.standardwebhooks.com/): `webhook-id`,
    /// `webhook-timestamp`, and `webhook-signature` (`v1,<base64>` over
    /// `id.timestamp.body`, several separated by spaces), with a
    /// `whsec_<base64>` secret.
    Standard { secret: String },
    /// `<header>: <prefix><HMAC-SHA256 of the body>` in `encoding`.
    HmacSha256 {
        secret: String,
        header: String,
        encoding: SignatureEncoding,
        prefix: Option<String>,
    },
    /// `Authorization: Bearer <token>`.
    Bearer { token: String },
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets never reach logs.
        let kind = match self {
            Self::GitHub { .. } => "GitHub",
            Self::Standard { .. } => "Standard",
            Self::HmacSha256 { .. } => "HmacSha256",
            Self::Bearer { .. } => "Bearer",
        };
        formatter.debug_struct(kind).finish_non_exhaustive()
    }
}

/// Why a request failed its check; the HTTP layer answers `401` for all.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum VerifyError {
    #[error("the request has no {0} header")]
    Missing(String),
    #[error("the signature header is malformed")]
    Malformed,
    #[error("the timestamp is more than five minutes from now")]
    StaleTimestamp,
    #[error("the signature does not match")]
    Mismatch,
    #[error("the webhook's secret is not usable: {0}")]
    BadSecret(&'static str),
}

/// Why a check is refused when a webhook is saved.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum VerifierError {
    #[error("a secret must be 1 to 1024 characters with no control characters")]
    Secret,
    #[error("a Standard Webhooks secret must be `whsec_` and base64 of 24 to 64 bytes")]
    StandardSecret,
    #[error("the signature header must be 1 to 128 visible ASCII characters without ':'")]
    Header,
    #[error("the signature header may not be Authorization, Cookie, or Content-Type")]
    ReservedHeader,
    #[error("a prefix must be at most 64 visible ASCII characters")]
    Prefix,
}

impl Verifier {
    /// Checks the verifier's shape, as when a webhook is saved.
    pub fn validate(&self) -> Result<(), VerifierError> {
        match self {
            Self::GitHub { secret } | Self::Bearer { token: secret } => plain_secret(secret),
            Self::Standard { secret } => standard_key(secret)
                .map(|_| ())
                .map_err(|_| VerifierError::StandardSecret),
            Self::HmacSha256 {
                secret,
                header,
                prefix,
                ..
            } => {
                plain_secret(secret)?;
                let valid_header = (1..=128).contains(&header.len())
                    && header
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic() && byte != b':');
                if !valid_header {
                    return Err(VerifierError::Header);
                }
                if ["authorization", "cookie", "content-type"]
                    .iter()
                    .any(|reserved| header.eq_ignore_ascii_case(reserved))
                {
                    return Err(VerifierError::ReservedHeader);
                }
                if let Some(prefix) = prefix
                    && (prefix.len() > 64 || !prefix.bytes().all(|byte| byte.is_ascii_graphic()))
                {
                    return Err(VerifierError::Prefix);
                }
                Ok(())
            }
        }
    }

    /// The header this check names a delivery by, if any (section 5.3).
    #[must_use]
    pub const fn delivery_header(&self) -> Option<DeliveryHeader> {
        match self {
            Self::GitHub { .. } => Some(DeliveryHeader::GitHub),
            Self::Standard { .. } => Some(DeliveryHeader::Standard),
            Self::HmacSha256 { .. } | Self::Bearer { .. } => None,
        }
    }

    /// The headers carrying this check's signature or token, which the
    /// model never sees, beside the ones [`super::render`] always hides.
    #[must_use]
    pub fn secret_headers(&self) -> Vec<String> {
        match self {
            Self::GitHub { .. } => vec!["x-hub-signature-256".to_owned()],
            Self::Standard { .. } => vec!["webhook-signature".to_owned()],
            Self::HmacSha256 { header, .. } => vec![header.to_ascii_lowercase()],
            Self::Bearer { .. } => vec!["authorization".to_owned()],
        }
    }

    /// Checks `request` at `now`.
    pub fn verify(&self, request: &WebhookRequest, now: Timestamp) -> Result<(), VerifyError> {
        match self {
            Self::GitHub { secret } => {
                let value = header(request, "x-hub-signature-256")?;
                let hex = value
                    .strip_prefix("sha256=")
                    .ok_or(VerifyError::Malformed)?;
                let tag = decode_hex(hex).ok_or(VerifyError::Malformed)?;
                verify_hmac(secret.as_bytes(), &request.body, &tag)
            }
            Self::Standard { secret } => verify_standard(secret, request, now),
            Self::HmacSha256 {
                secret,
                header: name,
                encoding,
                prefix,
            } => {
                let value = header(request, name)?;
                let signature = match prefix {
                    Some(prefix) => value
                        .strip_prefix(prefix.as_str())
                        .ok_or(VerifyError::Malformed)?,
                    None => value,
                };
                let tag = match encoding {
                    SignatureEncoding::Hex => decode_hex(signature),
                    SignatureEncoding::Base64 => decode_base64(signature),
                }
                .ok_or(VerifyError::Malformed)?;
                verify_hmac(secret.as_bytes(), &request.body, &tag)
            }
            Self::Bearer { token } => {
                let value = header(request, "authorization")?;
                let (scheme, given) = value.split_once(' ').ok_or(VerifyError::Malformed)?;
                if !scheme.eq_ignore_ascii_case("bearer") {
                    return Err(VerifyError::Malformed);
                }
                constant_time::verify_slices_are_equal(given.trim().as_bytes(), token.as_bytes())
                    .map_err(|_| VerifyError::Mismatch)
            }
        }
    }
}

/// The kinds of check, to generate a secret for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKind {
    /// `github`, `hmac_sha256`, and `bearer`: 32 random bytes, base64url.
    Plain,
    /// `standard`: 32 random bytes as `whsec_<base64>`.
    Standard,
}

/// A new secret of `kind`: 32 bytes from the OS's random source.
pub fn generate_secret(kind: SecretKind) -> Result<String, RandomError> {
    let bytes = random_secret_bytes()?;
    Ok(match kind {
        SecretKind::Plain => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        SecretKind::Standard => format!(
            "{STANDARD_SECRET_PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
    })
}

fn plain_secret(secret: &str) -> Result<(), VerifierError> {
    if (1..=1024).contains(&secret.len()) && !secret.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(VerifierError::Secret)
    }
}

fn standard_key(secret: &str) -> Result<Vec<u8>, VerifyError> {
    let encoded = secret
        .strip_prefix(STANDARD_SECRET_PREFIX)
        .ok_or(VerifyError::BadSecret("no whsec_ prefix"))?;
    let key = decode_base64(encoded).ok_or(VerifyError::BadSecret("not base64"))?;
    if (24..=64).contains(&key.len()) {
        Ok(key)
    } else {
        Err(VerifyError::BadSecret("not 24 to 64 bytes"))
    }
}

fn verify_standard(
    secret: &str,
    request: &WebhookRequest,
    now: Timestamp,
) -> Result<(), VerifyError> {
    let key = standard_key(secret)?;
    let id = header(request, "webhook-id")?;
    let timestamp = header(request, "webhook-timestamp")?;
    let signatures = header(request, "webhook-signature")?;
    let seconds = timestamp
        .trim()
        .parse::<i64>()
        .map_err(|_| VerifyError::Malformed)?;
    if seconds.abs_diff(now.as_second()) > STANDARD_TOLERANCE_SECONDS.unsigned_abs() {
        return Err(VerifyError::StaleTimestamp);
    }
    let mut signed = Vec::with_capacity(id.len() + timestamp.len() + request.body.len() + 2);
    signed.extend_from_slice(id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.trim().as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(&request.body);
    let mut any_v1 = false;
    for signature in signatures.split_ascii_whitespace() {
        let Some(encoded) = signature.strip_prefix("v1,") else {
            continue;
        };
        any_v1 = true;
        if let Some(tag) = decode_base64(encoded)
            && verify_hmac(&key, &signed, &tag).is_ok()
        {
            return Ok(());
        }
    }
    Err(if any_v1 {
        VerifyError::Mismatch
    } else {
        VerifyError::Malformed
    })
}

fn header<'a>(request: &'a WebhookRequest, name: &str) -> Result<&'a str, VerifyError> {
    request
        .header(name)
        .map(str::trim)
        .ok_or_else(|| VerifyError::Missing(name.to_owned()))
}

fn verify_hmac(key: &[u8], data: &[u8], tag: &[u8]) -> Result<(), VerifyError> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&key, data, tag).map_err(|_| VerifyError::Mismatch)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(text)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)], body: &str) -> WebhookRequest {
        WebhookRequest {
            method: "POST".to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    // GitHub's documented example ("Validating webhook deliveries").
    const GITHUB_SECRET: &str = "It's a Secret to Everybody";
    const GITHUB_BODY: &str = "Hello, World!";
    const GITHUB_SIGNATURE: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn github_signatures_match_the_documented_example() {
        let verifier = Verifier::GitHub {
            secret: GITHUB_SECRET.to_owned(),
        };
        let now = at(0);
        let signed = request(&[("X-Hub-Signature-256", GITHUB_SIGNATURE)], GITHUB_BODY);
        assert_eq!(verifier.verify(&signed, now), Ok(()));
        let upper = request(
            &[(
                "x-hub-signature-256",
                &GITHUB_SIGNATURE
                    .to_ascii_uppercase()
                    .replace("SHA256=", "sha256="),
            )],
            GITHUB_BODY,
        );
        assert_eq!(verifier.verify(&upper, now), Ok(()));
        let tampered = request(
            &[("X-Hub-Signature-256", GITHUB_SIGNATURE)],
            "Hello, World?",
        );
        assert_eq!(verifier.verify(&tampered, now), Err(VerifyError::Mismatch));
        assert_eq!(
            verifier.verify(&request(&[], GITHUB_BODY), now),
            Err(VerifyError::Missing("x-hub-signature-256".to_owned()))
        );
        let unprefixed = request(
            &[("X-Hub-Signature-256", &GITHUB_SIGNATURE[7..])],
            GITHUB_BODY,
        );
        assert_eq!(
            verifier.verify(&unprefixed, now),
            Err(VerifyError::Malformed)
        );
        let odd = request(&[("X-Hub-Signature-256", "sha256=abc")], GITHUB_BODY);
        assert_eq!(verifier.verify(&odd, now), Err(VerifyError::Malformed));
    }

    // The Standard Webhooks reference libraries' test vector.
    const STANDARD_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const STANDARD_ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const STANDARD_TIMESTAMP: i64 = 1_614_265_330;
    const STANDARD_BODY: &str = "{\"test\": 2432232314}";
    const STANDARD_SIGNATURE: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";

    fn standard_request(signature: &str, timestamp: i64) -> WebhookRequest {
        request(
            &[
                ("webhook-id", STANDARD_ID),
                ("webhook-timestamp", &timestamp.to_string()),
                ("webhook-signature", signature),
            ],
            STANDARD_BODY,
        )
    }

    #[test]
    fn standard_signatures_match_the_reference_vector() {
        let verifier = Verifier::Standard {
            secret: STANDARD_SECRET.to_owned(),
        };
        let now = at(STANDARD_TIMESTAMP);
        assert_eq!(
            verifier.verify(
                &standard_request(STANDARD_SIGNATURE, STANDARD_TIMESTAMP),
                now
            ),
            Ok(())
        );
        // Any one of several signatures, other versions skipped.
        let several = format!("v1,AAAA v1a,xyz {STANDARD_SIGNATURE}");
        assert_eq!(
            verifier.verify(&standard_request(&several, STANDARD_TIMESTAMP), now),
            Ok(())
        );
        assert_eq!(
            verifier.verify(&standard_request("v1,AAAA", STANDARD_TIMESTAMP), now),
            Err(VerifyError::Mismatch)
        );
        assert_eq!(
            verifier.verify(&standard_request("v2,AAAA", STANDARD_TIMESTAMP), now),
            Err(VerifyError::Malformed)
        );
        let mut tampered = standard_request(STANDARD_SIGNATURE, STANDARD_TIMESTAMP);
        tampered.body.push(b' ');
        assert_eq!(verifier.verify(&tampered, now), Err(VerifyError::Mismatch));
    }

    #[test]
    fn standard_timestamps_are_within_five_minutes_either_way() {
        let verifier = Verifier::Standard {
            secret: STANDARD_SECRET.to_owned(),
        };
        let signed = standard_request(STANDARD_SIGNATURE, STANDARD_TIMESTAMP);
        for (offset, ok) in [(-300, true), (300, true), (-301, false), (301, false)] {
            let result = verifier.verify(&signed, at(STANDARD_TIMESTAMP + offset));
            assert_eq!(result.is_ok(), ok, "{offset}");
            if !ok {
                assert_eq!(result, Err(VerifyError::StaleTimestamp));
            }
        }
        let garbled = standard_request(STANDARD_SIGNATURE, STANDARD_TIMESTAMP);
        let mut garbled = garbled;
        garbled.headers[1].1 = "soon".to_owned();
        assert_eq!(
            verifier.verify(&garbled, at(STANDARD_TIMESTAMP)),
            Err(VerifyError::Malformed)
        );
    }

    #[test]
    fn generic_hmac_takes_its_header_encoding_and_prefix() {
        let hex = "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        let tag = decode_hex(hex).unwrap();
        let base64_tag = base64::engine::general_purpose::STANDARD.encode(&tag);
        let cases = [
            (SignatureEncoding::Hex, None, hex.to_owned()),
            (
                SignatureEncoding::Hex,
                Some("sha256="),
                format!("sha256={hex}"),
            ),
            (SignatureEncoding::Base64, None, base64_tag.clone()),
            (
                SignatureEncoding::Base64,
                None,
                base64_tag.trim_end_matches('=').to_owned(),
            ),
            (
                SignatureEncoding::Base64,
                Some("v0="),
                format!("v0={base64_tag}"),
            ),
        ];
        for (encoding, prefix, value) in cases {
            let verifier = Verifier::HmacSha256 {
                secret: GITHUB_SECRET.to_owned(),
                header: "X-Signature".to_owned(),
                encoding,
                prefix: prefix.map(str::to_owned),
            };
            let signed = request(&[("x-signature", &value)], GITHUB_BODY);
            assert_eq!(verifier.verify(&signed, at(0)), Ok(()), "{value}");
            let wrong_body = request(&[("x-signature", &value)], "other");
            assert_eq!(
                verifier.verify(&wrong_body, at(0)),
                Err(VerifyError::Mismatch),
                "{value}"
            );
        }
        let prefixed = Verifier::HmacSha256 {
            secret: GITHUB_SECRET.to_owned(),
            header: "X-Signature".to_owned(),
            encoding: SignatureEncoding::Hex,
            prefix: Some("sha256=".to_owned()),
        };
        let bare = request(&[("X-Signature", hex)], GITHUB_BODY);
        assert_eq!(prefixed.verify(&bare, at(0)), Err(VerifyError::Malformed));
    }

    #[test]
    fn bearer_tokens_compare_exactly() {
        let verifier = Verifier::Bearer {
            token: "s3cret".to_owned(),
        };
        for (value, expected) in [
            ("Bearer s3cret", Ok(())),
            ("bearer s3cret", Ok(())),
            ("Bearer s3cre", Err(VerifyError::Mismatch)),
            ("Bearer s3cret2", Err(VerifyError::Mismatch)),
            ("Basic s3cret", Err(VerifyError::Malformed)),
            ("s3cret", Err(VerifyError::Malformed)),
        ] {
            assert_eq!(
                verifier.verify(&request(&[("Authorization", value)], ""), at(0)),
                expected,
                "{value}"
            );
        }
    }

    #[test]
    fn generated_secrets_are_accepted_and_differ() {
        let plain = generate_secret(SecretKind::Plain).unwrap();
        assert_eq!(plain.len(), 43);
        assert_ne!(plain, generate_secret(SecretKind::Plain).unwrap());
        assert!(Verifier::GitHub { secret: plain }.validate().is_ok());
        let standard = generate_secret(SecretKind::Standard).unwrap();
        assert!(standard.starts_with(STANDARD_SECRET_PREFIX));
        let verifier = Verifier::Standard { secret: standard };
        assert!(verifier.validate().is_ok());
    }

    #[test]
    fn verifiers_are_checked_when_saved() {
        let hmac = |header: &str, prefix: Option<&str>| Verifier::HmacSha256 {
            secret: "k".to_owned(),
            header: header.to_owned(),
            encoding: SignatureEncoding::Hex,
            prefix: prefix.map(str::to_owned),
        };
        assert!(hmac("X-Signature", Some("sha256=")).validate().is_ok());
        assert_eq!(hmac("", None).validate(), Err(VerifierError::Header));
        assert_eq!(
            hmac("Authorization", None).validate(),
            Err(VerifierError::ReservedHeader)
        );
        assert_eq!(
            hmac("X-S", Some("a b")).validate(),
            Err(VerifierError::Prefix)
        );
        assert_eq!(
            Verifier::GitHub {
                secret: String::new()
            }
            .validate(),
            Err(VerifierError::Secret)
        );
        assert_eq!(
            Verifier::Bearer {
                token: "a\nb".to_owned()
            }
            .validate(),
            Err(VerifierError::Secret)
        );
        assert_eq!(
            Verifier::Standard {
                secret: "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".to_owned()
            }
            .validate(),
            Err(VerifierError::StandardSecret)
        );
        assert_eq!(
            Verifier::Standard {
                secret: "whsec_AAAA".to_owned()
            }
            .validate(),
            Err(VerifierError::StandardSecret)
        );
        assert!(
            Verifier::Standard {
                secret: STANDARD_SECRET.to_owned()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn debug_output_hides_secrets() {
        let shown = format!(
            "{:?}",
            Verifier::Bearer {
                token: "s3cret".to_owned()
            }
        );
        assert!(!shown.contains("s3cret"), "{shown}");
    }
}
