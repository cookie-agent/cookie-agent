//! The `/plugins` panel: every configured plugin with its state, what it
//! declares, and why it last failed, and restarting one.

use cookie_agent_protocol::{PluginInfo, PluginState};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, paint_panel};

#[derive(Debug, Default)]
pub(super) struct PluginPanelState {
    /// `None` until `plugins.list` answers.
    pub(super) plugins: Option<Vec<PluginInfo>>,
    pub(super) selection: ListState,
    pub(super) refresh_in_flight: bool,
}

impl App {
    pub(super) fn open_plugin_panel(&mut self) {
        self.plugin_panel = PluginPanelState::default();
        self.modal = Modal::Plugins;
        self.poll_plugins();
    }

    /// Refetches `plugins.list` while the panel is open.
    pub(super) fn poll_plugins(&mut self) {
        if self.modal != Modal::Plugins || self.plugin_panel.refresh_in_flight {
            return;
        }
        self.plugin_panel.refresh_in_flight = true;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_plugins()
                .await
                .map(|listing| listing.plugins)
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PluginsRefreshed { result });
        });
    }

    pub(super) fn install_plugins(&mut self, result: Result<Vec<PluginInfo>, String>) {
        self.plugin_panel.refresh_in_flight = false;
        match result {
            Ok(plugins) => self.plugin_panel.plugins = Some(plugins),
            Err(error) => self.status = format!("plugin refresh failed: {error}"),
        }
    }

    pub(super) fn render_plugin_panel(&mut self, frame: &mut ratatui::Frame) {
        let area = centered(frame.area(), 80, 70);
        paint_panel(frame, area, &self.theme);
        let plugins = self.plugin_panel.plugins.as_deref().unwrap_or_default();
        let count = plugins.len();
        self.plugin_panel.selection.select((count > 0).then(|| {
            self.plugin_panel
                .selection
                .selected()
                .unwrap_or(0)
                .min(count - 1)
        }));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        let items = plugins
            .iter()
            .map(|plugin| {
                ListItem::new(Line::from(vec![
                    Span::styled(plugin.name.clone(), self.theme.body()),
                    Span::styled(
                        format!("  {}", state_label(plugin.state)),
                        self.theme.muted(),
                    ),
                ]))
            })
            .collect::<Vec<_>>();
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(self.theme.selected())
                .block(
                    crate::ui::panel_block()
                        .border_style(self.theme.panel_border())
                        .title(crate::ui::panel_title("Plugins"))
                        .title_bottom(crate::ui::panel_title(
                            Line::from(Span::styled(
                                "r restart | esc close",
                                self.theme.internal(),
                            ))
                            .right_aligned(),
                        )),
                ),
            chunks[0],
            &mut self.plugin_panel.selection,
        );
        let detail = match (
            self.plugin_panel.plugins.as_deref(),
            self.plugin_panel.selection.selected(),
        ) {
            (None, _) => "Loading plugins...".to_owned(),
            (Some([]), _) => {
                "No plugins are configured. Add one under [plugins.<name>] in the config."
                    .to_owned()
            }
            (Some(plugins), Some(index)) => {
                plugins.get(index).map(plugin_detail).unwrap_or_default()
            }
            (Some(_), None) => String::new(),
        };
        frame.render_widget(
            Paragraph::new(detail).wrap(Wrap { trim: false }).block(
                crate::ui::panel_block()
                    .border_style(self.theme.panel_border())
                    .title(crate::ui::panel_title("Plugin details")),
            ),
            chunks[1],
        );
    }

    pub(super) fn handle_plugin_key(&mut self, key: KeyEvent) {
        let count = self.plugin_panel.plugins.as_ref().map_or(0, Vec::len);
        match key.code {
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Up => {
                super::move_picker_selection(&mut self.plugin_panel.selection, count, true);
            }
            KeyCode::Down => {
                super::move_picker_selection(&mut self.plugin_panel.selection, count, false);
            }
            KeyCode::Char('r') => self.restart_selected_plugin(),
            _ => {}
        }
    }

    fn restart_selected_plugin(&mut self) {
        let Some(plugin) = self
            .plugin_panel
            .selection
            .selected()
            .and_then(|index| self.plugin_panel.plugins.as_ref()?.get(index))
        else {
            return;
        };
        let name = plugin.name.clone();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.status = format!("restarting plugin `{name}`…");
        self.spawn_rpc(async move {
            let update = match client.restart_plugin(name.clone()).await {
                Ok(_) => RpcUpdate::Status(format!("plugin `{name}` is restarting")),
                Err(error) => RpcUpdate::Status(error.to_string()),
            };
            let _ = updates.send(update);
        });
    }
}

const fn state_label(state: PluginState) -> &'static str {
    match state {
        PluginState::Disabled => "disabled",
        PluginState::Starting => "starting",
        PluginState::Connected => "connected",
        PluginState::Reconnecting => "reconnecting",
        PluginState::Failed => "failed",
        PluginState::Stopped => "stopped",
    }
}

fn plugin_detail(plugin: &PluginInfo) -> String {
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
        .map(|hook| {
            serde_json::to_value(hook)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    text.push_str(&format!("\nhooks: {}", list(&hooks)));
    let mut streams = Vec::new();
    if plugin.subscribe_events {
        streams.push("events".to_owned());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(state: PluginState) -> PluginInfo {
        PluginInfo {
            name: "issues".into(),
            state,
            reason: None,
            tools: Vec::new(),
            hooks: Vec::new(),
            subscribe_events: false,
            subscribe_bus: false,
            publish_bus: false,
            producer_messaging: false,
            dropped_events: 0,
            stderr: None,
        }
    }

    #[test]
    fn detail_shows_what_a_plugin_declares_and_why_it_failed() {
        let mut failed = plugin(PluginState::Failed);
        failed.reason = Some("exited with status 1".into());
        failed.tools = vec!["read_issue".into(), "close_issue".into()];
        failed.hooks = vec![cookie_agent_protocol::ExtensionInterceptionHook::ToolBeforeCall];
        failed.subscribe_events = true;
        failed.dropped_events = 3;
        failed.stderr = Some("token missing".into());
        let detail = plugin_detail(&failed);
        for expected in [
            "state: failed",
            "reason: exited with status 1",
            "tools: read_issue, close_issue",
            "hooks: tool_before_call",
            "also: events",
            "dropped notifications: 3",
            "stderr:\ntoken missing",
        ] {
            assert!(
                detail.contains(expected),
                "{expected:?} missing from {detail}"
            );
        }

        let quiet = plugin_detail(&plugin(PluginState::Connected));
        assert!(quiet.contains("tools: none"));
        assert!(!quiet.contains("reason:"));
        assert!(!quiet.contains("dropped"));
    }
}
