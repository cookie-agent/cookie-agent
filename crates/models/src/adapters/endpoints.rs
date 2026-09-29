use crate::{adapters::OvenAdapterFamily, authoring::EndpointUrl, recipes::EndpointPolicy};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaseUrlOverridePolicy {
    ManagedHttps,
    Forbidden,
    CustomHttpsOrReviewedLoopback { path: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EndpointBuildError {
    #[error("authored_base_url_forbidden")]
    AuthoredOverrideForbidden,
    #[error("endpoint_override_policy_violation")]
    Policy,
}

#[must_use]
pub const fn managed_base_url_policy(policy: EndpointPolicy) -> BaseUrlOverridePolicy {
    match policy {
        EndpointPolicy::DefaultWithAuthoredHttpsOverride { .. } => {
            BaseUrlOverridePolicy::ManagedHttps
        }
        EndpointPolicy::VertexPublisher
        | EndpointPolicy::BedrockRegional
        | EndpointPolicy::AzureOpenai => BaseUrlOverridePolicy::Forbidden,
    }
}

#[must_use]
pub const fn custom_endpoint_policy(family: OvenAdapterFamily) -> BaseUrlOverridePolicy {
    BaseUrlOverridePolicy::CustomHttpsOrReviewedLoopback {
        path: match family {
            OvenAdapterFamily::Anthropic
            | OvenAdapterFamily::AnthropicCompatible
            | OvenAdapterFamily::OpenaiChat
            | OvenAdapterFamily::OpenaiResponses
            | OvenAdapterFamily::OpenaiCompatible
            | OvenAdapterFamily::GoogleVertexGemini => "/v1",
            OvenAdapterFamily::GoogleGemini => "/v1beta",
            OvenAdapterFamily::AwsBedrockConverse
            | OvenAdapterFamily::AzureOpenaiChat
            | OvenAdapterFamily::AzureOpenaiResponses => "/",
            OvenAdapterFamily::CohereV2Chat => "/v2",
        },
    }
}

pub fn validate_managed_base_url(
    policy: EndpointPolicy,
    authored: Option<&EndpointUrl>,
) -> Result<(), EndpointBuildError> {
    let Some(authored) = authored else {
        return Ok(());
    };
    match managed_base_url_policy(policy) {
        BaseUrlOverridePolicy::ManagedHttps => {
            let parsed = Url::parse(authored.as_str()).map_err(|_| EndpointBuildError::Policy)?;
            if parsed.scheme() == "https" || parsed.scheme() == "http" && loopback_host(&parsed) {
                Ok(())
            } else {
                Err(EndpointBuildError::Policy)
            }
        }
        BaseUrlOverridePolicy::Forbidden => Err(EndpointBuildError::AuthoredOverrideForbidden),
        BaseUrlOverridePolicy::CustomHttpsOrReviewedLoopback { .. } => unreachable!(),
    }
}

/// Whether `url` is an `http`/`https` URL on `localhost`, `127.0.0.1`, or
/// `::1`: a local server, such as LM Studio, Ollama, or QVAC, that may be
/// reached over plain HTTP and without credentials.
#[must_use]
pub fn is_loopback_url(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https") && loopback_host(&parsed))
}

fn loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

pub fn validate_custom_endpoint(
    family: OvenAdapterFamily,
    endpoint: &EndpointUrl,
) -> Result<(), EndpointBuildError> {
    let parsed = Url::parse(endpoint.as_str()).map_err(|_| EndpointBuildError::Policy)?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    let BaseUrlOverridePolicy::CustomHttpsOrReviewedLoopback { path } =
        custom_endpoint_policy(family)
    else {
        unreachable!()
    };
    if parsed.scheme() == "http"
        && loopback_host(&parsed)
        && parsed.port().is_some()
        && parsed.path() == path
    {
        Ok(())
    } else {
        Err(EndpointBuildError::Policy)
    }
}
