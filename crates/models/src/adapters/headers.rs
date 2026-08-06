use std::collections::{BTreeMap, BTreeSet};

use crate::{
    adapters::OvenAdapterFamily,
    authoring::{AuthDefinition, HeaderName, SafeStaticHeaderValue},
    recipes::{auth_method, validate_auth_definition},
};

const TRANSPORT_OWNED: &[&str] = &[
    "accept",
    "authorization",
    "connection",
    "content-length",
    "content-type",
    "cookie",
    "host",
    "proxy-authorization",
    "set-cookie",
    "transfer-encoding",
    "user-agent",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StaticHeaderError {
    #[error("unsupported_auth_method")]
    Auth,
    #[error("static_header_owned")]
    Owned,
}

pub fn validate_static_headers(
    family: OvenAdapterFamily,
    auth: &AuthDefinition,
    headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
) -> Result<(), StaticHeaderError> {
    let method = validate_auth_definition(auth, family.allowed_auth_methods())
        .map_err(|_| StaticHeaderError::Auth)?;
    let mut owned = TRANSPORT_OWNED.iter().copied().collect::<BTreeSet<_>>();
    owned.extend(protocol_headers(family));
    owned.extend(method.owned_headers.iter().copied());
    if method.id == "api-key-header-v1" {
        let header_name = auth
            .parameters
            .iter()
            .find(|(name, _)| name.as_str() == "header_name")
            .map(|(_, value)| value.as_str())
            .ok_or(StaticHeaderError::Auth)?;
        owned.insert(header_name);
    }
    if headers.keys().any(|header| owned.contains(header.as_str())) {
        return Err(StaticHeaderError::Owned);
    }
    debug_assert!(auth_method(method.id).is_some());
    Ok(())
}

fn protocol_headers(family: OvenAdapterFamily) -> &'static [&'static str] {
    match family {
        OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => {
            &["anthropic-version", "anthropic-beta", "x-api-key"]
        }
        OvenAdapterFamily::GoogleGemini => &["x-goog-api-key"],
        OvenAdapterFamily::AwsBedrockConverse => &[
            "authorization",
            "host",
            "x-amz-date",
            "x-amz-content-sha256",
            "x-amz-security-token",
        ],
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses => {
            &["api-key"]
        }
        OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::OpenaiCompatible
        | OvenAdapterFamily::GoogleVertexGemini
        | OvenAdapterFamily::CohereV2Chat => &[],
    }
}
