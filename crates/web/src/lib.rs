//! The cookie agent web client: a Leptos page rendered in the browser
//! (client-side), speaking the client protocol through
//! `cookie_agent_protocol`'s browser transport. `scripts/build-web.sh`
//! builds it with Trunk; `cookie daemon` serves the result.
//!
//! The crate also compiles natively, so the workspace builds without the
//! wasm target; its tests cover the pure logic only.

mod app;
pub mod browser;
pub mod commands;
pub mod connect;
pub mod connection;
mod highlight;
pub mod keys;
mod media;
pub mod render;
pub mod theme;
mod views;
pub mod workspace;

pub use app::App;

/// Renders the page into `<body>`.
pub fn start() {
    leptos::mount::mount_to_body(App);
}
