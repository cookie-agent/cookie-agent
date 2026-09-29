//! Cookie-side plumbing shared by every Oven adapter: resolved
//! authentication, header templates, routing discriminators, transport
//! timeouts, and the shared HTTP client. Per-family settings and request
//! options are built directly as Oven types by the executable compiler.

use std::{
    collections::{BTreeMap, HashMap},
    env,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::Duration,
};

use http::{HeaderMap, HeaderName, HeaderValue};
use oven_sdk::{
    ApiEndpoint, HeaderConfig, HeaderContext, HeaderOverrides, HeaderProvider, ModelError,
    ProviderConfig, ProviderId, SecretString,
};
use oven_sdk_anthropic::{AnthropicAuth, AnthropicCompatibleAuth, AnthropicTimeouts};
use oven_sdk_azure::{AzureOpenAiAuth, AzureOpenAiTimeouts};
use oven_sdk_bedrock::{AwsCredentials, BedrockAuth, BedrockTimeouts};
use oven_sdk_cohere::{CohereAuth, CohereTimeouts};
use oven_sdk_google::{GoogleApiKeyAuth, GoogleTimeouts};
use oven_sdk_google_vertex::{GoogleVertexTimeouts, VertexAuth};
use oven_sdk_openai::{OpenAiAuth, OpenAiCompatibleAuth, OpenAiTimeouts};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::Zeroize as _;

/// Provider identity, endpoint, and header templates shared by every adapter
/// constructor; each family pairs it with its own authentication.
#[derive(Clone)]
pub(crate) struct CommonProvider {
    id: ProviderId,
    api: ApiEndpoint,
    headers: HeaderConfig,
}

impl CommonProvider {
    pub(crate) fn new(
        provider_id: &str,
        endpoint: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self, ModelBuildError> {
        Ok(Self {
            id: ProviderId::new(provider_id),
            api: ApiEndpoint::parse(endpoint)?,
            headers: header_config(headers)?,
        })
    }

    pub(crate) fn with_auth<A>(&self, auth: A) -> ProviderConfig<A> {
        ProviderConfig::new(
            self.id.clone(),
            self.api.clone(),
            auth,
            self.headers.clone(),
        )
        .expect("common provider identity was validated")
    }
}

/// Resolved authentication, zeroized on drop. Oven never reads the environment.
pub(crate) enum AuthConfig {
    /// No adapter-injected authentication.
    None,
    /// API-key authentication.
    ApiKey { value: String },
    /// Bearer authentication.
    Bearer { token: String },
    /// Caller-selected reviewed API-key header authentication.
    HeaderApiKey { name: String, value: String },
    /// Official OpenAI bearer authentication and account headers.
    Openai {
        api_key: String,
        organization: Option<String>,
        project: Option<String>,
    },
    /// Static OAuth access token.
    AccessToken { token: String },
    /// Static AWS credentials.
    AwsStatic {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

impl Drop for AuthConfig {
    fn drop(&mut self) {
        match self {
            Self::None => {}
            Self::ApiKey { value } => value.zeroize(),
            Self::Bearer { token } | Self::AccessToken { token } => token.zeroize(),
            Self::HeaderApiKey { value, .. } => value.zeroize(),
            Self::Openai { api_key, .. } => api_key.zeroize(),
            Self::AwsStatic {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                access_key_id.zeroize();
                secret_access_key.zeroize();
                if let Some(session_token) = session_token {
                    session_token.zeroize();
                }
            }
        }
    }
}

impl AuthConfig {
    pub(crate) fn anthropic(&self) -> Result<AnthropicAuth, ModelBuildError> {
        match self {
            Self::None => Ok(AnthropicAuth::None),
            Self::ApiKey { value } => Ok(AnthropicAuth::ApiKey(secret(value))),
            _ => Err(wrong_auth("anthropic", "none or api_key")),
        }
    }

    pub(crate) fn anthropic_compatible(&self) -> Result<AnthropicCompatibleAuth, ModelBuildError> {
        match self {
            Self::None => Ok(AnthropicCompatibleAuth::None),
            Self::ApiKey { value } => Ok(AnthropicCompatibleAuth::ApiKey(secret(value))),
            Self::Bearer { token } => Ok(AnthropicCompatibleAuth::Bearer(secret(token))),
            _ => Err(wrong_auth(
                "Anthropic-compatible",
                "none, api_key, or bearer",
            )),
        }
    }

    pub(crate) fn openai(&self) -> Result<OpenAiAuth, ModelBuildError> {
        match self {
            Self::Openai {
                api_key,
                organization,
                project,
            } => Ok(OpenAiAuth {
                api_key: secret(api_key),
                organization: organization.clone(),
                project: project.clone(),
            }),
            _ => Err(wrong_auth("OpenAI", "openai")),
        }
    }

    pub(crate) fn openai_compatible(&self) -> Result<OpenAiCompatibleAuth, ModelBuildError> {
        match self {
            Self::None => Ok(OpenAiCompatibleAuth::none()),
            Self::Bearer { token } => Ok(OpenAiCompatibleAuth::bearer(secret(token))),
            Self::HeaderApiKey { name, value } => Ok(OpenAiCompatibleAuth::headers(Arc::new(
                SecretHeaderProvider {
                    name: name.clone(),
                    value: secret(value),
                },
            ))),
            _ => Err(wrong_auth(
                "OpenAI-compatible",
                "none, bearer, or header_api_key",
            )),
        }
    }

    pub(crate) fn google(&self) -> Result<GoogleApiKeyAuth, ModelBuildError> {
        match self {
            Self::ApiKey { value } => Ok(GoogleApiKeyAuth::new(value.clone())),
            _ => Err(wrong_auth("Google", "api_key")),
        }
    }

    pub(crate) fn vertex(&self) -> Result<VertexAuth, ModelBuildError> {
        match self {
            Self::AccessToken { token } => Ok(VertexAuth::AccessToken(secret(token))),
            _ => Err(wrong_auth("Vertex", "access_token")),
        }
    }

    pub(crate) fn bedrock(&self) -> Result<BedrockAuth, ModelBuildError> {
        match self {
            Self::AwsStatic {
                access_key_id,
                secret_access_key,
                session_token,
            } => Ok(BedrockAuth::Static(AwsCredentials {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
            })),
            _ => Err(wrong_auth("Bedrock", "aws_static")),
        }
    }

    pub(crate) fn azure(&self) -> Result<AzureOpenAiAuth, ModelBuildError> {
        match self {
            Self::ApiKey { value } => Ok(AzureOpenAiAuth::ApiKey(secret(value))),
            Self::Bearer { token } => {
                let token = token.clone();
                Ok(AzureOpenAiAuth::Entra(Arc::new(move || {
                    let token = token.clone();
                    Box::pin(async move { Ok(token) })
                })))
            }
            _ => Err(wrong_auth("Azure OpenAI", "api_key or bearer")),
        }
    }

    pub(crate) fn cohere(&self) -> Result<CohereAuth, ModelBuildError> {
        match self {
            Self::Bearer { token } => Ok(CohereAuth::bearer(secret(token))),
            _ => Err(wrong_auth("Cohere", "bearer")),
        }
    }
}

#[derive(Clone)]
struct SecretHeaderProvider {
    name: String,
    value: SecretString,
}

impl HeaderProvider for SecretHeaderProvider {
    fn headers(&self, _context: &HeaderContext) -> Result<HeaderOverrides, ModelError> {
        let name = HeaderName::from_bytes(self.name.as_bytes())
            .map_err(|_| ModelError::invalid_request("invalid API-key header name"))?;
        let value = HeaderValue::from_str(self.value.expose_secret())
            .map_err(|_| ModelError::invalid_request("invalid API-key header value"))?;
        Ok(HeaderOverrides::new(HeaderMap::from_iter([(name, value)])))
    }
}

#[derive(Clone)]
pub(crate) struct TemplateHeaderProvider {
    templates: Vec<(HeaderName, HeaderTemplate)>,
}

#[derive(Clone)]
enum HeaderTemplate {
    Static(Option<HeaderValue>),
    Dynamic(Vec<HeaderTemplateSegment>),
}

#[derive(Clone)]
enum HeaderTemplateSegment {
    Literal(String),
    SessionId,
    ParentSessionId,
    Env {
        name: String,
        default: Option<String>,
    },
}

impl TemplateHeaderProvider {
    pub(crate) fn new(values: &BTreeMap<String, String>) -> Result<Self, ModelBuildError> {
        let templates = values
            .iter()
            .map(|(name, value)| {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| ModelBuildError::HeaderName(name.clone()))?;
                let template = parse_header_template(value)
                    .map_err(|()| ModelBuildError::HeaderTemplate(name.clone()))?;
                let template = match template {
                    ParsedHeaderTemplate::Static(value) => HeaderTemplate::Static(
                        (!value.is_empty())
                            .then(|| HeaderValue::from_str(&value))
                            .transpose()
                            .map_err(|_| ModelBuildError::HeaderValue(name.clone()))?,
                    ),
                    ParsedHeaderTemplate::Dynamic(segments) => HeaderTemplate::Dynamic(segments),
                };
                Ok((header_name, template))
            })
            .collect::<Result<_, ModelBuildError>>()?;
        Ok(Self { templates })
    }

    pub(crate) fn resolved_headers(
        &self,
        context: &HeaderContext,
    ) -> Result<HeaderMap, &'static str> {
        let mut headers = HeaderMap::new();
        for (name, template) in &self.templates {
            let value = match template {
                HeaderTemplate::Static(value) => value.clone(),
                HeaderTemplate::Dynamic(segments) => {
                    let value = resolve_header_template(segments, context)?;
                    (!value.is_empty())
                        .then(|| HeaderValue::from_str(&value))
                        .transpose()
                        .map_err(|_| "invalid configured header value")?
                }
            };
            if let Some(value) = value {
                headers.insert(name.clone(), value);
            }
        }
        Ok(headers)
    }
}

impl HeaderProvider for TemplateHeaderProvider {
    fn headers(&self, context: &HeaderContext) -> Result<HeaderOverrides, ModelError> {
        self.resolved_headers(context)
            .map(HeaderOverrides::new)
            .map_err(ModelError::invalid_request)
    }
}

enum ParsedHeaderTemplate {
    Static(String),
    Dynamic(Vec<HeaderTemplateSegment>),
}

fn parse_header_template(template: &str) -> Result<ParsedHeaderTemplate, ()> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut cursor = 0;
    let mut dynamic = false;
    let push_literal = |segments: &mut Vec<HeaderTemplateSegment>, literal: &mut String| {
        if !literal.is_empty() {
            segments.push(HeaderTemplateSegment::Literal(std::mem::take(literal)));
        }
    };
    while cursor < template.len() {
        let remaining = &template[cursor..];
        if let Some(text) = remaining.strip_prefix("$$") {
            literal.push('$');
            cursor = template.len() - text.len();
        } else if let Some(text) = remaining.strip_prefix("${session_id}") {
            push_literal(&mut segments, &mut literal);
            segments.push(HeaderTemplateSegment::SessionId);
            dynamic = true;
            cursor = template.len() - text.len();
        } else if let Some(text) = remaining.strip_prefix("${parent_session_id}") {
            push_literal(&mut segments, &mut literal);
            segments.push(HeaderTemplateSegment::ParentSessionId);
            dynamic = true;
            cursor = template.len() - text.len();
        } else if let Some(expression) = remaining.strip_prefix("${env:") {
            let end = expression.find('}').ok_or(())?;
            let expression_body = &expression[..end];
            let (name, default) = expression_body
                .split_once(":-")
                .map_or((expression_body, None), |(name, default)| {
                    (name, Some(default))
                });
            if !valid_env_name(name) {
                return Err(());
            }
            push_literal(&mut segments, &mut literal);
            segments.push(HeaderTemplateSegment::Env {
                name: name.to_owned(),
                default: default.map(str::to_owned),
            });
            dynamic = true;
            cursor += 6 + end + 1;
        } else {
            let character = remaining.chars().next().expect("nonempty remainder");
            literal.push(character);
            cursor += character.len_utf8();
        }
    }
    if !dynamic {
        return Ok(ParsedHeaderTemplate::Static(literal));
    }
    push_literal(&mut segments, &mut literal);
    Ok(ParsedHeaderTemplate::Dynamic(segments))
}

fn resolve_header_template(
    segments: &[HeaderTemplateSegment],
    context: &HeaderContext,
) -> Result<String, &'static str> {
    let mut output = String::new();
    for segment in segments {
        match segment {
            HeaderTemplateSegment::Literal(literal) => output.push_str(literal),
            HeaderTemplateSegment::SessionId => output.push_str(&context.session_id),
            HeaderTemplateSegment::ParentSessionId => {
                output.push_str(context.parent_session_id.as_deref().unwrap_or_default());
            }
            HeaderTemplateSegment::Env { name, default } => match env::var(name) {
                Ok(value) => output.push_str(&value),
                Err(env::VarError::NotPresent) => {
                    output.push_str(
                        default
                            .as_deref()
                            .ok_or("missing header environment variable")?,
                    );
                }
                Err(env::VarError::NotUnicode(_)) => {
                    return Err("header environment variable is not UTF-8");
                }
            },
        }
    }
    Ok(output)
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && (name.as_bytes()[0].is_ascii_uppercase() || name.as_bytes()[0] == b'_')
        && name
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Concrete model construction error with redacted formatting.
#[derive(Error)]
pub enum ModelBuildError {
    #[error("invalid model configuration: {0}")]
    Oven(#[source] Box<ModelError>),
    #[error("invalid HTTP header name `{0}`")]
    HeaderName(String),
    #[error("invalid HTTP header value for `{0}`")]
    HeaderValue(String),
    #[error("invalid HTTP header template for `{0}`")]
    HeaderTemplate(String),
    #[error("{adapter} adapter requires auth.type = {expected}")]
    WrongAuth {
        adapter: &'static str,
        expected: &'static str,
    },
    #[error("could not encode provider request defaults")]
    ProviderOptions(#[source] serde_json::Error),
}

impl From<ModelError> for ModelBuildError {
    fn from(error: ModelError) -> Self {
        Self::Oven(Box::new(error))
    }
}

impl std::fmt::Debug for ModelBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ModelBuildError")
            .field(&self.to_string())
            .finish()
    }
}

pub(crate) fn wrong_auth(adapter: &'static str, expected: &'static str) -> ModelBuildError {
    ModelBuildError::WrongAuth { adapter, expected }
}

fn secret(value: &str) -> SecretString {
    SecretString::new(value.to_owned())
}

fn header_config(values: &BTreeMap<String, String>) -> Result<HeaderConfig, ModelBuildError> {
    // TODO: Once cookie-agent can advance Oven without a release-integrity
    // ripple, consume dynamic HeaderOverrides with into_map instead of cloning.
    Ok(HeaderConfig {
        static_headers: HeaderOverrides::new(HeaderMap::new()),
        dynamic_headers: if values.is_empty() {
            None
        } else {
            Some(Arc::new(TemplateHeaderProvider::new(values)?))
        },
    })
}

pub(crate) fn header_routing_discriminator(values: &BTreeMap<String, String>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cookie-agent/header-templates/v1\0");
    for (name, value) in values {
        digest.update(name.as_bytes());
        digest.update(b"\0");
        digest.update(value.as_bytes());
        digest.update(b"\0");
    }
    format!("{:x}", digest.finalize())
}

pub(crate) fn combined_routing_discriminator(
    routing: Option<&str>,
    headers: Option<&str>,
) -> Option<String> {
    match (routing, headers) {
        (Some(routing), Some(headers)) => {
            let mut digest = Sha256::new();
            digest.update(b"cookie-agent/request-routing/v1\0");
            digest.update(
                serde_json::to_vec(&(routing, headers)).expect("routing strings serialize"),
            );
            Some(format!("{:x}", digest.finalize()))
        }
        (routing, headers) => routing.or(headers).map(str::to_owned),
    }
}

/// Encodes typed request options as one provider-options namespace.
pub(crate) fn namespace(
    name: &str,
    value: impl Serialize,
) -> Result<BTreeMap<String, Value>, ModelBuildError> {
    let value = serde_json::to_value(value).map_err(ModelBuildError::ProviderOptions)?;
    Ok([(name.to_owned(), value)].into_iter().collect())
}

/// Transport phase timeouts applied to every adapter.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TimeoutsConfig {
    pub connect: Duration,
    /// Headers bound time-to-first-byte, which is also where slow providers
    /// spend their thinking time on large contexts; keep it well above long
    /// TTFTs.
    pub headers: Duration,
    pub credentials: Duration,
    pub stream_idle: Duration,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            headers: Duration::from_secs(300),
            credentials: Duration::from_secs(30),
            stream_idle: Duration::from_secs(120),
        }
    }
}

/// One HTTP client per connect timeout, shared by every compiled model.
/// Building a client loads the platform's root certificates, which, done once
/// per model, dominated startup; a `reqwest::Client` is a cheap handle to one
/// shared connection pool. `None` if the client cannot be built, in which case
/// an adapter builds its own and reports the failure itself.
fn shared_http_client(connect: Duration) -> Option<reqwest::Client> {
    static CLIENTS: OnceLock<Mutex<HashMap<Duration, reqwest::Client>>> = OnceLock::new();
    let mut clients = CLIENTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(client) = clients.get(&connect) {
        return Some(client.clone());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(connect)
        .build()
        .ok()?;
    clients.insert(connect, client.clone());
    Some(client)
}

impl TimeoutsConfig {
    pub(crate) fn shared_client(self) -> Option<reqwest::Client> {
        shared_http_client(self.connect)
    }
    pub(crate) fn anthropic(self) -> AnthropicTimeouts {
        AnthropicTimeouts {
            headers: self.headers,
            credentials: self.credentials,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn openai(self) -> OpenAiTimeouts {
        OpenAiTimeouts {
            connect: self.connect,
            headers: self.headers,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn google(self) -> GoogleTimeouts {
        GoogleTimeouts {
            connect: self.connect,
            headers: self.headers,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn vertex(self) -> GoogleVertexTimeouts {
        GoogleVertexTimeouts {
            connect: self.connect,
            headers: self.headers,
            credentials: self.credentials,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn bedrock(self) -> BedrockTimeouts {
        BedrockTimeouts {
            connect: self.connect,
            headers: self.headers,
            credentials: self.credentials,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn azure(self) -> AzureOpenAiTimeouts {
        AzureOpenAiTimeouts {
            connect: self.connect,
            headers: self.headers,
            credentials: self.credentials,
            stream_idle: self.stream_idle,
        }
    }
    pub(crate) fn cohere(self) -> CohereTimeouts {
        CohereTimeouts {
            connect: self.connect,
            headers: self.headers,
            stream_idle: self.stream_idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_routing_discriminator_composes_with_effective_header_templates() {
        let route = Some("header:x-api-key");
        let one = header_routing_discriminator(&BTreeMap::from([(
            "x-route".into(),
            "route-one-secret".into(),
        )]));
        let two = header_routing_discriminator(&BTreeMap::from([(
            "x-route".into(),
            "route-two-secret".into(),
        )]));
        let first = combined_routing_discriminator(route, Some(&one)).unwrap();
        assert_ne!(
            first,
            combined_routing_discriminator(route, Some(&two)).unwrap()
        );
        assert_ne!(first, combined_routing_discriminator(route, None).unwrap());
        assert!(!first.contains("route-one-secret"));
        assert_ne!(
            first,
            combined_routing_discriminator(Some("header:api-key"), Some(&one)).unwrap()
        );
    }

    #[test]
    fn session_header_templates_resolve_from_request_context() {
        let headers = TemplateHeaderProvider::new(&BTreeMap::from([
            ("x-session-id".into(), "${session_id}".into()),
            ("x-session-parent-id".into(), "${parent_session_id}".into()),
        ]))
        .unwrap();
        let root = headers.headers(&HeaderContext::new("root")).unwrap();
        assert_eq!(
            root.as_map().get("x-session-id").unwrap(),
            &HeaderValue::from_static("root")
        );
        assert!(!root.as_map().contains_key("x-session-parent-id"));
        let child = headers
            .headers(&HeaderContext::new("child").with_parent_session_id("root"))
            .unwrap();
        assert_eq!(child.as_map()["x-session-parent-id"], "root");
    }

    #[test]
    fn literal_header_templates_are_prevalidated_and_stored_as_values() {
        let headers = TemplateHeaderProvider::new(&BTreeMap::from([
            ("x-literal".into(), "fixed$$value".into()),
            ("x-empty".into(), String::new()),
        ]))
        .unwrap();
        assert!(
            headers
                .templates
                .iter()
                .all(|(_, template)| matches!(template, HeaderTemplate::Static(_)))
        );
        let resolved = headers.headers(&HeaderContext::new("unused")).unwrap();
        assert_eq!(resolved.as_map()["x-literal"], "fixed$value");
        assert!(!resolved.as_map().contains_key("x-empty"));
    }

    #[test]
    fn malformed_header_templates_fail_during_model_construction() {
        let error = TemplateHeaderProvider::new(&BTreeMap::from([(
            "x-invalid".into(),
            "${env:UNFINISHED".into(),
        )]))
        .err()
        .expect("invalid template");
        assert!(matches!(error, ModelBuildError::HeaderTemplate(name) if name == "x-invalid"));
    }
}
