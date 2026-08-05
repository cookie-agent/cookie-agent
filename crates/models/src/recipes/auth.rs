use std::collections::{BTreeMap, BTreeSet};

use cookie_agent_identity::{AuthFieldName, AuthParameterId};
use serde::Serialize;

use crate::authoring::{AuthDefinition, AuthOverride, SafeAuthParameterValue, SecretString};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    AccessToken,
    AccessKeyId,
    SecretAccessKey,
    SessionToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AuthCredential {
    pub name: &'static str,
    pub kind: CredentialKind,
    pub required: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAuth {
    None,
    BearerApiKey,
    HeaderApiKey,
    AnthropicApiKey,
    GoogleApiKey,
    OAuthAccessToken,
    AwsSigV4Static,
    AzureApiKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AuthMethodRecipe {
    pub id: &'static str,
    pub credentials: &'static [AuthCredential],
    pub wire: WireAuth,
    pub owned_headers: &'static [&'static str],
}

const API_KEY: &[AuthCredential] = &[AuthCredential {
    name: "api_key",
    kind: CredentialKind::ApiKey,
    required: true,
}];
const ACCESS_TOKEN: &[AuthCredential] = &[AuthCredential {
    name: "access_token",
    kind: CredentialKind::AccessToken,
    required: true,
}];
const AWS_STATIC: &[AuthCredential] = &[
    AuthCredential {
        name: "access_key_id",
        kind: CredentialKind::AccessKeyId,
        required: true,
    },
    AuthCredential {
        name: "secret_access_key",
        kind: CredentialKind::SecretAccessKey,
        required: true,
    },
    AuthCredential {
        name: "session_token",
        kind: CredentialKind::SessionToken,
        required: false,
    },
];

const METHODS: &[AuthMethodRecipe] = &[
    AuthMethodRecipe {
        id: "no-auth-v1",
        credentials: &[],
        wire: WireAuth::None,
        owned_headers: &[],
    },
    AuthMethodRecipe {
        id: "bearer-api-key-v1",
        credentials: API_KEY,
        wire: WireAuth::BearerApiKey,
        owned_headers: &["authorization"],
    },
    AuthMethodRecipe {
        id: "api-key-header-v1",
        credentials: API_KEY,
        wire: WireAuth::HeaderApiKey,
        owned_headers: &[],
    },
    AuthMethodRecipe {
        id: "anthropic-api-key-v1",
        credentials: API_KEY,
        wire: WireAuth::AnthropicApiKey,
        owned_headers: &["x-api-key"],
    },
    AuthMethodRecipe {
        id: "google-api-key-header-v1",
        credentials: API_KEY,
        wire: WireAuth::GoogleApiKey,
        owned_headers: &["x-goog-api-key"],
    },
    AuthMethodRecipe {
        id: "oauth-access-token-v1",
        credentials: ACCESS_TOKEN,
        wire: WireAuth::OAuthAccessToken,
        owned_headers: &["authorization"],
    },
    AuthMethodRecipe {
        id: "aws-sigv4-credentials-v1",
        credentials: AWS_STATIC,
        wire: WireAuth::AwsSigV4Static,
        owned_headers: &[
            "authorization",
            "host",
            "x-amz-date",
            "x-amz-content-sha256",
            "x-amz-security-token",
        ],
    },
    AuthMethodRecipe {
        id: "azure-api-key-v1",
        credentials: API_KEY,
        wire: WireAuth::AzureApiKey,
        owned_headers: &["api-key"],
    },
];

#[must_use]
pub fn auth_method(id: &str) -> Option<&'static AuthMethodRecipe> {
    METHODS.iter().find(|method| method.id == id)
}

pub fn auth_methods() -> impl ExactSizeIterator<Item = &'static AuthMethodRecipe> {
    METHODS.iter()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AuthValidationError {
    #[error("unsupported_auth_method")]
    UnsupportedAuthMethod,
    #[error("invalid_auth_shape")]
    InvalidShape,
    #[error("ambiguous_api_key")]
    AmbiguousApiKey,
}

pub fn validate_auth_override(
    auth: &AuthOverride,
    allowed: &[&str],
) -> Result<&'static AuthMethodRecipe, AuthValidationError> {
    validate(
        auth.method.as_str(),
        &BTreeMap::new(),
        &auth.values,
        allowed,
    )
}

pub fn validate_auth_definition(
    auth: &AuthDefinition,
    allowed: &[&str],
) -> Result<&'static AuthMethodRecipe, AuthValidationError> {
    validate(
        auth.method.as_str(),
        &auth.parameters,
        &auth.values,
        allowed,
    )
}

fn validate(
    id: &str,
    parameters: &BTreeMap<AuthParameterId, SafeAuthParameterValue>,
    values: &BTreeMap<AuthFieldName, SecretString>,
    allowed: &[&str],
) -> Result<&'static AuthMethodRecipe, AuthValidationError> {
    if !allowed.contains(&id) {
        return Err(AuthValidationError::UnsupportedAuthMethod);
    }
    let method = auth_method(id).ok_or(AuthValidationError::UnsupportedAuthMethod)?;
    let actual = values
        .keys()
        .map(AuthFieldName::as_str)
        .collect::<BTreeSet<_>>();
    let required = method
        .credentials
        .iter()
        .filter(|field| field.required)
        .map(|field| field.name)
        .collect::<BTreeSet<_>>();
    let permitted = method
        .credentials
        .iter()
        .map(|field| field.name)
        .collect::<BTreeSet<_>>();
    if !required.is_subset(&actual) || !actual.is_subset(&permitted) {
        return Err(AuthValidationError::InvalidShape);
    }
    if id == "api-key-header-v1" {
        let header = parameters
            .iter()
            .find(|(name, _)| name.as_str() == "header_name")
            .map(|(_, value)| value.as_str());
        if parameters.len() != 1 || !matches!(header, Some("x-api-key" | "api-key")) {
            return Err(AuthValidationError::InvalidShape);
        }
    } else if !parameters.is_empty() {
        return Err(AuthValidationError::InvalidShape);
    }
    Ok(method)
}
