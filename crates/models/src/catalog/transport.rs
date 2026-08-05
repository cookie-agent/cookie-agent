use std::{future::Future, pin::Pin, time::Duration};

use futures_util::{Stream, StreamExt as _};
use reqwest_oven::{Client, header};
use thiserror::Error;

use super::{CATALOG_MAX_BYTES, MODELS_DEV_CATALOG_URL};

pub const MODELS_DEV_USER_AGENT: &str = "cookie-agent/0.1 catalog-cache/1";

pub type CatalogBodyStream =
    Pin<Box<dyn Stream<Item = Result<Vec<u8>, CatalogTransportError>> + Send>>;
pub type CatalogTransportFuture<'a> = Pin<
    Box<dyn Future<Output = Result<CatalogTransportResponse, CatalogTransportError>> + Send + 'a>,
>;

/// The only request shape accepted by the catalog transport boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRequest {
    pub url: &'static str,
    pub user_agent: &'static str,
    pub accept: &'static str,
    pub accept_encoding: &'static str,
    pub if_none_match: Option<String>,
}

impl CatalogRequest {
    pub(crate) fn fixed(etag: Option<String>) -> Self {
        Self {
            url: MODELS_DEV_CATALOG_URL,
            user_agent: MODELS_DEV_USER_AGENT,
            accept: "application/json",
            accept_encoding: "identity",
            if_none_match: etag,
        }
    }
}

/// Streamed transport response. Tests inject this without live network access.
pub struct CatalogTransportResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub body: CatalogBodyStream,
}

impl CatalogTransportResponse {
    #[must_use]
    pub fn from_bytes(status: u16, bytes: Vec<u8>) -> Self {
        let length = bytes.len() as u64;
        Self {
            status,
            content_type: Some("application/json".to_owned()),
            content_encoding: None,
            content_length: Some(length),
            etag: None,
            body: Box::pin(futures_util::stream::once(async move { Ok(bytes) })),
        }
    }

    #[must_use]
    pub fn not_modified() -> Self {
        Self {
            status: 304,
            content_type: None,
            content_encoding: None,
            content_length: Some(0),
            etag: None,
            body: Box::pin(futures_util::stream::empty()),
        }
    }
}

/// Injectable fixed catalog transport.
pub trait CatalogTransport: Send + Sync {
    fn fetch(&self, request: CatalogRequest) -> CatalogTransportFuture<'_>;
}

/// Production rustls client with redirects and automatic content coding disabled.
#[derive(Clone, Debug)]
pub struct HttpCatalogTransport {
    client: Client,
}

impl HttpCatalogTransport {
    pub fn new() -> Result<Self, CatalogTransportError> {
        let client = Client::builder()
            .redirect(reqwest_oven::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent(MODELS_DEV_USER_AGENT)
            .build()
            .map_err(|_| CatalogTransportError::ClientBuild)?;
        Ok(Self { client })
    }
}

impl CatalogTransport for HttpCatalogTransport {
    fn fetch(&self, request: CatalogRequest) -> CatalogTransportFuture<'_> {
        Box::pin(async move {
            if request.url != MODELS_DEV_CATALOG_URL
                || request.user_agent != MODELS_DEV_USER_AGENT
                || request.accept != "application/json"
                || request.accept_encoding != "identity"
            {
                return Err(CatalogTransportError::InvalidRequest);
            }
            let mut builder = self
                .client
                .get(MODELS_DEV_CATALOG_URL)
                .header(header::ACCEPT, request.accept)
                .header(header::ACCEPT_ENCODING, request.accept_encoding);
            if let Some(etag) = request.if_none_match {
                let value = header::HeaderValue::from_str(&etag)
                    .map_err(|_| CatalogTransportError::InvalidEtag)?;
                builder = builder.header(header::IF_NONE_MATCH, value);
            }
            let response = builder
                .send()
                .await
                .map_err(|_| CatalogTransportError::RequestFailed)?;
            let status = response.status().as_u16();
            let headers = response.headers();
            let content_type = header_string(headers, header::CONTENT_TYPE)?;
            let content_encoding = header_string(headers, header::CONTENT_ENCODING)?;
            let etag = header_string(headers, header::ETAG)?;
            let content_length = match headers.get(header::CONTENT_LENGTH) {
                Some(value) => Some(
                    value
                        .to_str()
                        .map_err(|_| CatalogTransportError::InvalidHeaders)?
                        .parse::<u64>()
                        .map_err(|_| CatalogTransportError::InvalidHeaders)?,
                ),
                None => None,
            };
            if content_length.is_some_and(|length| length > CATALOG_MAX_BYTES as u64) {
                return Err(CatalogTransportError::BodyTooLarge);
            }
            let body = response.bytes_stream().map(|chunk| {
                chunk
                    .map(|bytes| bytes.to_vec())
                    .map_err(|_| CatalogTransportError::BodyReadFailed)
            });
            Ok(CatalogTransportResponse {
                status,
                content_type,
                content_encoding,
                content_length,
                etag,
                body: Box::pin(body),
            })
        })
    }
}

fn header_string(
    headers: &header::HeaderMap,
    name: header::HeaderName,
) -> Result<Option<String>, CatalogTransportError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| CatalogTransportError::InvalidHeaders)
        })
        .transpose()
}

/// Body-free transport errors.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CatalogTransportError {
    #[error("catalog transport client could not be constructed")]
    ClientBuild,
    #[error("catalog transport request shape is invalid")]
    InvalidRequest,
    #[error("catalog cache ETag is invalid")]
    InvalidEtag,
    #[error("catalog response headers are invalid")]
    InvalidHeaders,
    #[error("catalog request failed")]
    RequestFailed,
    #[error("catalog response body read failed")]
    BodyReadFailed,
    #[error("catalog response body exceeds the byte limit")]
    BodyTooLarge,
}
