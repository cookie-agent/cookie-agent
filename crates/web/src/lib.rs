//! The Cookie Agent web client: a Leptos page rendered in the browser
//! (client-side), speaking the client protocol through
//! `cookie_agent_protocol`'s browser transport. `scripts/build-web.sh`
//! builds it with Trunk; `cookie daemon` serves the result.
//!
//! The crate also compiles natively, so the workspace builds without the
//! wasm target; its tests cover the pure logic only.

pub mod agents;
mod app;
pub mod attach;
pub mod browser;
pub mod cards;
pub mod commands;
pub mod connect;
pub mod connection;
pub mod errors;
mod files;
pub mod format;
pub mod goal;
mod highlight;
pub mod icons;
pub mod install;
pub mod keys;
pub mod layout;
pub mod machines;
mod media;
pub mod memory;
pub mod modes;
pub mod places;
pub mod render;
pub mod skills;
pub mod theme;
pub mod tree;
mod views;
pub mod workspace;

pub use app::App;

/// Renders the page into `<body>`.
pub fn start() {
    leptos::mount::mount_to_body(App);
}
