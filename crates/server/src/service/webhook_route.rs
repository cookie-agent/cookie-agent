//! Webhooks on the daemon's listeners (`docs/site/specs/webhooks.md`
//! section 5): `POST /hooks/<token>`, on the main listener and the public
//! one. No daemon token, ticket, or origin is checked: the URL token, and a
//! webhook's own signature check, are the only keys. Every unknown,
//! disabled, or switched-off URL answers the same `404`.
//!
//! The route answers `405` for other methods, `404` for a URL that names no
//! receiving webhook, and `413` for a body over `[webhooks] max_body_bytes`
//! once the limit is read; the engine does the rest
//! ([`Engine::receive_webhook`]). Tokens are never logged.
//!
//! [`Engine::receive_webhook`]: cookie_agent_engine::Engine::receive_webhook

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::Response,
};
use cookie_agent_engine::{WebhookReply, WebhookRequest};

use super::websocket::WebSocketState;

fn answer(reply: &WebhookReply) -> Response {
    let mut response = Response::new(Body::from(reply.body.to_string()));
    *response.status_mut() =
        StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(seconds) = reply.retry_after {
        headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

/// Serves `/hooks/<token>`.
pub(crate) async fn receive(
    State(state): State<WebSocketState>,
    Path(token): Path<String>,
    request: Request,
) -> Response {
    if request.method() != Method::POST {
        let mut response = answer(&WebhookReply {
            status: 405,
            body: serde_json::json!({"error": "method_not_allowed"}),
            retry_after: None,
        });
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    let Some(server) = state.server.get() else {
        return answer(&WebhookReply::not_found());
    };
    let engine = &server.engine;
    if !engine.webhook_receives(&token) {
        return answer(&WebhookReply::not_found());
    }
    let (parts, body) = request.into_parts();
    let limit = usize::try_from(engine.webhook_body_limit()).unwrap_or(usize::MAX);
    let Ok(body) = axum::body::to_bytes(body, limit).await else {
        return answer(&WebhookReply {
            status: 413,
            body: serde_json::json!({"error": "too_large"}),
            retry_after: None,
        });
    };
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    let reply = engine
        .receive_webhook(
            &token,
            WebhookRequest {
                method: parts.method.as_str().to_owned(),
                headers,
                body: body.to_vec(),
            },
        )
        .await;
    answer(&reply)
}

#[cfg(test)]
mod tests;
