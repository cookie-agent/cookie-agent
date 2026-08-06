use crate::{
    adapters::OvenAdapterFamily,
    authoring::EndpointUrl,
    recipes::{EndpointPolicy, ValidatedSetup},
};
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
    #[error("invalid_endpoint_setup")]
    Setup,
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
            if parsed.scheme() == "https" {
                Ok(())
            } else {
                Err(EndpointBuildError::Policy)
            }
        }
        BaseUrlOverridePolicy::Forbidden => Err(EndpointBuildError::AuthoredOverrideForbidden),
        BaseUrlOverridePolicy::CustomHttpsOrReviewedLoopback { .. } => unreachable!(),
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
    let host = parsed.host_str().ok_or(EndpointBuildError::Policy)?;
    let exact_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if parsed.scheme() == "http"
        && exact_loopback
        && parsed.port().is_some()
        && parsed.path() == path
    {
        Ok(())
    } else {
        Err(EndpointBuildError::Policy)
    }
}

pub fn build_endpoint(
    policy: EndpointPolicy,
    authored: Option<&EndpointUrl>,
    setup: &ValidatedSetup,
) -> Result<String, EndpointBuildError> {
    validate_managed_base_url(policy, authored)?;
    match policy {
        EndpointPolicy::DefaultWithAuthoredHttpsOverride { default } => Ok(authored
            .map(EndpointUrl::as_str)
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_owned()),
        EndpointPolicy::VertexPublisher => {
            if authored.is_some() {
                return Err(EndpointBuildError::AuthoredOverrideForbidden);
            }
            let project = field(setup, "project")?;
            let location = field(setup, "location")?;
            let resource = field(setup, "resource")?;
            Ok(format!(
                "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/{resource}"
            ))
        }
        EndpointPolicy::BedrockRegional => {
            if authored.is_some() {
                return Err(EndpointBuildError::AuthoredOverrideForbidden);
            }
            Ok(format!(
                "https://bedrock-runtime.{}.amazonaws.com",
                field(setup, "region")?
            ))
        }
        EndpointPolicy::AzureOpenai => {
            if authored.is_some() {
                return Err(EndpointBuildError::AuthoredOverrideForbidden);
            }
            Ok(format!(
                "https://{}.openai.azure.com",
                field(setup, "resource_name")?
            ))
        }
    }
}

fn field<'a>(setup: &'a ValidatedSetup, id: &str) -> Result<&'a str, EndpointBuildError> {
    setup
        .values
        .get(id)
        .map(String::as_str)
        .ok_or(EndpointBuildError::Setup)
}
