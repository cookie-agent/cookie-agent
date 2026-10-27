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
pub mod badges;
pub mod browser;
pub mod cards;
pub mod commands;
pub mod connect;
pub mod connection;
pub mod cron;
pub mod emoji;
pub mod errors;
mod files;
pub mod follow;
pub mod format;
pub mod goal;
mod highlight;
pub mod icons;
pub mod install;
pub mod keys;
pub mod layout;
pub mod machines;
pub mod media;
pub mod memory;
pub mod modes;
pub mod oauth;
pub mod panics;
pub mod places;
pub mod push;
pub mod render;
pub mod session_list;
pub mod skills;
pub mod theme;
pub mod tree;
mod views;
pub mod workspace;

pub use app::App;

/// Renders the page into `<body>`, with panics logged to the console.
#[cfg(target_arch = "wasm32")]
pub fn start() {
    panics::install();
    leptos::mount::mount_to_body(App);
}
