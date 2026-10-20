//! The page's entry point, which Trunk builds for the browser.

fn main() {
    #[cfg(target_arch = "wasm32")]
    cookie_agent_web::start();
}
