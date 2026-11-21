//! Speech to text through the OpenAI-compatible `POST /audio/transcriptions`
//! API (`docs/site/specs/voice-input.md` sections 5 and 6).
//!
//! A [`TranscriptionTarget`] is where one backend's request goes: a base URL
//! and the headers it carries, credentials included. It comes from a
//! configured provider ([`crate::CompiledModelRuntime::transcription_target`])
//! or from an endpoint and auth written for the backend
//! ([`TranscriptionTarget::endpoint`]). Credentials never leave this crate in
//! a readable form: the target's headers are marked sensitive, and it formats
//! without them.

use std::{
    collections::BTreeMap,
    fmt,
    sync::OnceLock,
    time::{Duration, Instant},
};

use oven_sdk::HeaderContext;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use zeroize::Zeroizing;

use cookie_agent_identity::AuthParameterId;

use crate::{
    AuthDefinition, EndpointUrl, HeaderName, SafeStaticHeaderValue,
    adapters::oven::TemplateHeaderProvider,
    compiler::{ExecutableCredentialMaterial, global_header_base, merge_headers_onto},
};

/// The longest error text kept from a backend's answer.
const MAX_ERROR_TEXT: usize = 300;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a backend could not transcribe. Formats without credentials.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TranscriptionError {
    #[error("provider `{0}` is not configured")]
    NotConfigured(String),
    #[error("provider `{0}` has no credentials")]
    NoCredentials(String),
    #[error("its sign-in method `{0}` cannot be used for transcription")]
    UnsupportedAuth(String),
    #[error("its headers are invalid: {0}")]
    Headers(String),
    #[error("its URL is invalid")]
    Url,
    #[error("unreachable: {0}")]
    Unreachable(String),
    #[error("timed out")]
    Timeout,
    #[error("returned {status}{}", status_detail(.message))]
    Status {
        status: u16,
        message: Option<String>,
    },
    #[error("answered without a transcript: {0}")]
    BadAnswer(String),
}

fn status_detail(message: &Option<String>) -> String {
    message
        .as_deref()
        .map(|text| format!(" ({text})"))
        .unwrap_or_default()
}

/// Where one backend's request goes: `<base>/audio/transcriptions`, with
/// these headers.
#[derive(Clone)]
pub struct TranscriptionTarget {
    base: String,
    headers: HeaderMap,
}

impl fmt::Debug for TranscriptionTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranscriptionTarget")
            .field("base", &self.base)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl TranscriptionTarget {
    /// A backend written with its own `endpoint` and `auth` (none: no
    /// auth), with the global and the backend's own headers.
    pub fn endpoint(
        endpoint: &EndpointUrl,
        auth: Option<&AuthDefinition>,
        global_headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
        headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
    ) -> Result<Self, TranscriptionError> {
        let material = ExecutableCredentialMaterial {
            method: auth
                .map_or("no-auth-v1", |auth| auth.method.as_str())
                .to_owned(),
            values: auth
                .map(|auth| {
                    auth.values
                        .iter()
                        .map(|(field, value)| (field.clone(), value.expose().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            token_source: None,
        };
        let header_name = auth.and_then(|auth| {
            auth.parameters
                .get(&AuthParameterId::new("header_name").expect("static id"))
                .map(|value| value.as_str().to_owned())
        });
        Self::from_parts(
            endpoint.as_str(),
            &material,
            header_name.as_deref(),
            global_headers,
            &[headers],
        )
    }

    pub(crate) fn from_parts(
        base: &str,
        material: &ExecutableCredentialMaterial,
        header_name: Option<&str>,
        global_headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
        layers: &[&BTreeMap<HeaderName, SafeStaticHeaderValue>],
    ) -> Result<Self, TranscriptionError> {
        let merged = merge_headers_onto(
            &global_header_base(global_headers)
                .map_err(|error| TranscriptionError::Headers(error.to_string()))?,
            layers
                .iter()
                .map(|layer| (*layer, "speech backend".to_owned())),
        )
        .map_err(|error| TranscriptionError::Headers(error.to_string()))?;
        let templates = merged
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
            .collect::<BTreeMap<_, _>>();
        let mut resolved = TemplateHeaderProvider::new(&templates)
            .map_err(|error| TranscriptionError::Headers(error.to_string()))?
            .resolved_headers(&HeaderContext::new(String::new()))
            .map_err(|reason| TranscriptionError::Headers(reason.to_owned()))?;
        if let Some((name, value)) = auth_header(material, header_name)? {
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| TranscriptionError::Headers("invalid credential".to_owned()))?;
            value.set_sensitive(true);
            resolved.insert(name, value);
        }
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            headers: resolved,
        })
    }

    /// The URL the request goes to.
    #[must_use]
    pub fn url(&self) -> String {
        format!("{}/audio/transcriptions", self.base)
    }
}

/// The header a credential travels in, by its auth method; `None` for no
/// auth.
fn auth_header(
    material: &ExecutableCredentialMaterial,
    header_name: Option<&str>,
) -> Result<Option<(reqwest::header::HeaderName, Zeroizing<String>)>, TranscriptionError> {
    let value = |field: &str| {
        material
            .values
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .map(|(_, value)| Zeroizing::new(value.clone()))
            .ok_or_else(|| TranscriptionError::Headers(format!("missing credential `{field}`")))
    };
    let named = |name: &str| {
        reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| TranscriptionError::Headers(format!("invalid header name `{name}`")))
    };
    Ok(Some(match material.method.as_str() {
        "no-auth-v1" => return Ok(None),
        "bearer-api-key-v1" => (
            AUTHORIZATION,
            Zeroizing::new(format!("Bearer {}", value("api_key")?.as_str())),
        ),
        "oauth-access-token-v1" => (
            AUTHORIZATION,
            Zeroizing::new(format!("Bearer {}", value("access_token")?.as_str())),
        ),
        "api-key-header-v1" => (named(header_name.unwrap_or("api-key"))?, value("api_key")?),
        "anthropic-api-key-v1" => (named("x-api-key")?, value("api_key")?),
        "google-api-key-header-v1" => (named("x-goog-api-key")?, value("api_key")?),
        "azure-api-key-v1" => (named("api-key")?, value("api_key")?),
        other => return Err(TranscriptionError::UnsupportedAuth(other.to_owned())),
    }))
}

/// One recording to transcribe.
#[derive(Clone, Copy, Debug)]
pub struct TranscriptionRequest<'a> {
    pub audio: &'a [u8],
    /// The recording's media type, such as `audio/webm`.
    pub media_type: &'a str,
    /// The file name the service sees; its extension names the format.
    pub filename: &'a str,
    pub model: &'a str,
    pub language: Option<&'a str>,
    pub prompt: Option<&'a str>,
}

/// What a backend answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transcript {
    /// The words, trimmed; empty when it heard none.
    pub text: String,
    /// How long the request took.
    pub elapsed: Duration,
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("the transcription HTTP client builds")
    })
}

/// Sends `request` to `target`, waiting at most `timeout` when one is given.
pub async fn transcribe(
    target: &TranscriptionTarget,
    request: TranscriptionRequest<'_>,
    timeout: Option<Duration>,
) -> Result<Transcript, TranscriptionError> {
    let url = reqwest::Url::parse(&target.url()).map_err(|_| TranscriptionError::Url)?;
    let (content_type, body) = multipart(request);
    let mut builder = client()
        .post(url)
        .headers(target.headers.clone())
        .header(CONTENT_TYPE, content_type)
        .body(body);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    let started = Instant::now();
    let response = builder.send().await.map_err(request_error)?;
    let status = response.status();
    let body = response.bytes().await.map_err(request_error)?;
    if !status.is_success() {
        return Err(TranscriptionError::Status {
            status: status.as_u16(),
            message: error_text(&body),
        });
    }
    let text = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("text")?.as_str().map(str::to_owned))
        .ok_or_else(|| TranscriptionError::BadAnswer(short(&String::from_utf8_lossy(&body))))?;
    Ok(Transcript {
        text: text.trim().to_owned(),
        elapsed: started.elapsed(),
    })
}

fn request_error(error: reqwest::Error) -> TranscriptionError {
    if error.is_timeout() {
        TranscriptionError::Timeout
    } else {
        // Without the URL: a query could carry a key.
        TranscriptionError::Unreachable(short(&error.without_url().to_string()))
    }
}

/// The message of an error answer: OpenAI's `error.message`, a plain
/// `error` or `detail`, else the start of the body.
fn error_text(body: &[u8]) -> Option<String> {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let message = parsed.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .or_else(|| value.get("error"))
            .or_else(|| value.get("detail"))
            .or_else(|| value.get("message"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    let text = message.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| short(&text))
}

fn short(text: &str) -> String {
    if text.chars().count() <= MAX_ERROR_TEXT {
        return text.to_owned();
    }
    let mut cut = text.chars().take(MAX_ERROR_TEXT).collect::<String>();
    cut.push('…');
    cut
}

/// The `multipart/form-data` body: `file`, `model`, `response_format`, and
/// `language` and `prompt` when given.
fn multipart(request: TranscriptionRequest<'_>) -> (String, Vec<u8>) {
    let mut random = [0u8; 16];
    let _ = getrandom::getrandom(&mut random);
    let boundary = format!(
        "cookie-speech-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let mut body = Vec::with_capacity(request.audio.len() + 1024);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model", request.model);
    field("response_format", "json");
    if let Some(language) = request.language {
        field("language", language);
    }
    if let Some(prompt) = request.prompt {
        field("prompt", prompt);
    }
    let filename = request.filename.replace(['"', '\r', '\n'], "_");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            request.media_type
        )
        .as_bytes(),
    );
    body.extend_from_slice(request.audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[cfg(test)]
mod tests;
