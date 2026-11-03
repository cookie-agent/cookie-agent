//! The `/plugins` panel's rules every client shares: a plugin's state and
//! what it declares, as text, and how a plugin's events read in the status
//! line.

use cookie_agent_protocol::{ExtensionBusEventParams, PluginInfo, PluginState};

/// The cookie the web page keeps the daemon's page ticket in
/// (`ServerHello::page_ticket`).
pub const PAGE_TICKET_COOKIE: &str = "cookie_page_ticket";

/// Whether the daemon at client endpoint `daemon_url` (`ws://host/ws`)
/// serves the web page at origin `page_origin` (`http://host`): only then
/// do its plugin pages open below the page, with the page's cookie.
#[must_use]
pub fn serves_page(daemon_url: &str, page_origin: &str) -> bool {
    let Some((scheme, rest)) = daemon_url.split_once("://") else {
        return false;
    };
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" => "http",
        "wss" => "https",
        _ => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{authority}").eq_ignore_ascii_case(page_origin.trim_end_matches('/'))
}

/// Where a plugin's pages open, from the web page at `page_url` and the
/// plugin's `ui_path` (`docs/site/specs/plugin-web-ui.md` section 7).
#[must_use]
pub fn plugin_page_url(page_url: &str, ui_path: &str) -> String {
    let page = page_url.split('#').next().unwrap_or(page_url);
    let separator = if page.ends_with('/') { "" } else { "/" };
    format!("{page}{separator}{ui_path}")
}

pub const fn state_label(state: PluginState) -> &'static str {
    match state {
        PluginState::Disabled => "disabled",
        PluginState::Starting => "starting",
        PluginState::Connected => "connected",
        PluginState::Reconnecting => "reconnecting",
        PluginState::Failed => "failed",
        PluginState::Stopped => "stopped",
        PluginState::Unknown => "unknown",
    }
}

pub fn plugin_detail(plugin: &PluginInfo, page_url: Option<&str>) -> String {
    let mut text = format!("{}\nstate: {}", plugin.name, state_label(plugin.state));
    if let Some(reason) = &plugin.reason {
        text.push_str(&format!("\nreason: {reason}"));
    }
    if let Some(ui_path) = &plugin.ui_path {
        let pages = match page_url {
            Some(page_url) => plugin_page_url(page_url, ui_path),
            None => format!("/{ui_path}"),
        };
        text.push_str(&format!("\npages: {pages}"));
    }
    let list = |items: &[String]| {
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join(", ")
        }
    };
    text.push_str(&format!("\ntools: {}", list(&plugin.tools)));
    let hooks = plugin
        .hooks
        .iter()
        .map(|hook| hook.as_str().to_owned())
        .collect::<Vec<_>>();
    text.push_str(&format!("\nhooks: {}", list(&hooks)));
    let mut streams = Vec::new();
    if !plugin.subscribe_events.is_empty() {
        let patterns = plugin
            .subscribe_events
            .iter()
            .map(cookie_agent_protocol::EventPattern::as_str)
            .collect::<Vec<_>>();
        streams.push(format!("events ({})", patterns.join(", ")));
    }
    if plugin.subscribe_bus {
        streams.push("bus".to_owned());
    }
    if plugin.publish_bus {
        streams.push("publishes bus".to_owned());
    }
    if plugin.producer_messaging {
        streams.push("producers".to_owned());
    }
    text.push_str(&format!("\nalso: {}", list(&streams)));
    if plugin.dropped_events > 0 {
        text.push_str(&format!(
            "\ndropped notifications: {}",
            plugin.dropped_events
        ));
    }
    if let Some(stderr) = &plugin.stderr {
        text.push_str("\n\nstderr:\n");
        text.push_str(stderr);
    }
    text
}

/// The status line a plugin event of the viewed session gives.
pub fn plugin_event_status(event: &ExtensionBusEventParams) -> String {
    format!("plugin {}: {}", event.plugin, event.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_daemon_at_the_pages_origin_serves_its_plugin_pages() {
        assert!(serves_page(
            "ws://127.0.0.1:7777/ws",
            "http://127.0.0.1:7777"
        ));
        assert!(serves_page("wss://Box.example/ws", "https://box.example"));
        assert!(!serves_page(
            "ws://127.0.0.1:7778/ws",
            "http://127.0.0.1:7777"
        ));
        assert!(!serves_page("wss://box.example/ws", "http://box.example"));
        assert!(!serves_page("ws://other.example/ws", "http://box.example"));
        assert!(!serves_page("box.example/ws", "http://box.example"));
    }

    #[test]
    fn plugin_pages_open_below_the_page() {
        assert_eq!(
            plugin_page_url("https://box.example/", "plugins/gcal/"),
            "https://box.example/plugins/gcal/"
        );
        assert_eq!(
            plugin_page_url("https://box.example/sub#session=x", "plugins/gcal/"),
            "https://box.example/sub/plugins/gcal/"
        );
    }
}
