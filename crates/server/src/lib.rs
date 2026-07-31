//! Axum daemon and WebSocket transport scaffolding.

use axum::{Router, http::StatusCode, routing::get};

pub fn app() -> Router {
    Router::new().route("/ws", get(ws_placeholder))
}

async fn ws_placeholder() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

pub async fn daemon() -> anyhow::Result<()> {
    let _app = app();
    Ok(())
}
