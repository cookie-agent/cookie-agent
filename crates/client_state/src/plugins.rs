//! The `/plugins` panel's rules every client shares: a plugin's state and
//! what it declares, as text, and how a plugin's events read in the status
//! line.

use cookie_agent_protocol::{ExtensionBusEventParams, PluginInfo, PluginState};

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

pub fn plugin_detail(plugin: &PluginInfo) -> String {
    let mut text = format!("{}\nstate: {}", plugin.name, state_label(plugin.state));
    if let Some(reason) = &plugin.reason {
        text.push_str(&format!("\nreason: {reason}"));
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
