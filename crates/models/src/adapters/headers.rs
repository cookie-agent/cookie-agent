use std::collections::BTreeMap;

use crate::{
    adapters::OvenAdapterFamily,
    authoring::{AuthDefinition, HeaderName, SafeStaticHeaderValue},
    recipes::validate_auth_definition,
};

const OWNED: &[&str] = &[
    "accept",
    "anthropic-beta",
    "anthropic-version",
    "connection",
    "content-length",
    "content-type",
    "host",
    "proxy-authorization",
    "transfer-encoding",
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
    validate_auth_definition(auth, family.allowed_auth_methods())
        .map_err(|_| StaticHeaderError::Auth)?;
    if headers
        .keys()
        .any(|header| OWNED.contains(&header.as_str()) || header.as_str().starts_with("x-amz-"))
    {
        return Err(StaticHeaderError::Owned);
    }
    Ok(())
}
