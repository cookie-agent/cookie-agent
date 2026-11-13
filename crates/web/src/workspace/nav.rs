//! Moving around the page: the sidebar's views, the Settings sections,
//! the sidebar itself, and the history entries that let browser Back (and
//! a phone's Back gesture) close what is open, one layer at a time.

use leptos::prelude::*;

use super::{Panel, Workspace};
use crate::{
    browser,
    connect::SIDEBAR_KEY,
    layout::{HistoryStep, Layer, SettingsSection, View, reconcile},
};

impl Workspace {
    /// Shows `view`, reading what it lists; `section` picks the Settings
    /// section. It closes any sheet and the sidebar shown as the page.
    pub fn open_view(self, view: View, section: Option<SettingsSection>) {
        if self.is_guest() {
            return;
        }
        self.panel.set(None);
        self.sidebar_open.set(false);
        // An asset's page closes with its view, and opening the view shows
        // the list.
        self.asset_page.set(None);
        match view {
            View::Chat => {}
            View::Agents => self.open_config(),
            View::Skills => self.poll_skill_list(),
            View::Assets => self.open_assets(),
            View::Sharing => self.refresh_shares(),
            View::Mcp => self.poll_mcp(),
            View::Settings => self.open_section(section),
            View::Machines => self.poll_runtimes(),
            View::Memory => self.open_memory(),
            View::Cron => self.open_crons(),
            View::Webhooks => self.open_webhooks(),
        }
        self.view.set(view);
    }

    /// A tap on one of the sidebar's views: the view, at its list where it
    /// has one; the sidebar shown as the page gives way to it.
    pub fn open_from_sidebar(self, view: View) {
        self.open_view(view, None);
    }

    /// Opens a Settings section (`None`: the list, on a phone), reading
    /// what it lists.
    pub fn open_section(self, section: Option<SettingsSection>) {
        match section {
            None | Some(SettingsSection::Configuration) => self.open_config(),
            Some(SettingsSection::Plugins) => self.poll_plugins(),
            Some(SettingsSection::Notifications) => self.poll_notifications(),
            Some(
                SettingsSection::Providers
                | SettingsSection::Display
                | SettingsSection::Browser
                | SettingsSection::About,
            ) => {}
        }
        self.settings_section.set(section);
    }

    /// Reads again what the open view and sheet show, keeping what is
    /// open in them, for a redialed connection: a read made while the
    /// page reconnected was not sent, one on its way failed with the
    /// connection, and the daemon may have changed meanwhile.
    /// [`Workspace::refresh_lists`] reads the runtimes (the Machines view,
    /// Settings → Providers, the runtimes sheet), the projects (Move to
    /// project…), and the cron jobs; the redial reads the viewed session's
    /// approvals, permission mode, and cost.
    pub(super) fn reread_open(self) {
        match self.view.get_untracked() {
            View::Agents => self.poll_config(),
            View::Settings => match untrack(|| self.shown_section()) {
                None | Some(SettingsSection::Configuration) => self.poll_config(),
                Some(SettingsSection::Plugins) => self.poll_plugins(),
                Some(SettingsSection::Notifications) => self.poll_notifications(),
                Some(
                    SettingsSection::Providers
                    | SettingsSection::Display
                    | SettingsSection::Browser
                    | SettingsSection::About,
                ) => {}
            },
            View::Memory => self.poll_memory(),
            View::Skills => self.poll_skill_list(),
            View::Assets => self.poll_assets(),
            View::Sharing => self.refresh_shares(),
            View::Mcp => self.poll_mcp(),
            View::Chat | View::Machines | View::Cron | View::Webhooks => {}
        }
        match self.panel.get_untracked() {
            Some(Panel::Skills) => self.load_skills(),
            Some(Panel::Usage) => self.load_usage(),
            Some(Panel::Permissions) => self.load_permissions(),
            Some(Panel::Project) => {
                // An editor that read its project keeps its edits.
                let unread = self.project_editor.with_untracked(|editor| {
                    editor
                        .as_ref()
                        .filter(|editor| !editor.agents_md_loaded)
                        .and_then(|editor| editor.project.as_ref())
                        .map(|project| project.id.clone())
                });
                if let Some(project) = unread {
                    self.load_editor(project);
                }
            }
            // The rest show the store, the runtime snapshot, or what
            // the redial reads anyway.
            _ => {}
        }
    }

    /// The Settings section a window shows: a phone's list shows none, a
    /// wider window always shows one.
    pub fn shown_section(self) -> Option<SettingsSection> {
        let section = self.settings_section.get();
        if self.narrow.get() {
            section
        } else {
            Some(section.unwrap_or(SettingsSection::Configuration))
        }
    }

    /// Whether the page shows `view` (and in Settings, `section`) now.
    pub(super) fn showing(self, view: View, section: Option<SettingsSection>) -> bool {
        self.view.get_untracked() == view
            && (section.is_none() || untrack(|| self.shown_section()) == section)
    }

    /// The sidebar button: a desktop pins or hides the sidebar, remembered
    /// in the browser. A narrower window shows the sidebar as the page, or
    /// hides it; on a phone's second level (an item open in a view's list)
    /// it goes back up to the list instead.
    pub fn toggle_sidebar(self) {
        let view = self.view.get_untracked();
        if self.desktop.get_untracked() {
            let shown = !self.sidebar_pinned.get_untracked();
            browser::store(SIDEBAR_KEY, if shown { "shown" } else { "hidden" });
            self.sidebar_pinned.set(shown);
        } else if view != View::Chat && self.narrow.get_untracked() && !untrack(|| self.at_list()) {
            self.close_layer(Layer::Editor);
            self.close_layer(Layer::Section);
        } else {
            self.sidebar_open.update(|open| *open = !*open);
        }
    }

    /// Whether the open view shows its first level: its list (Skills, MCP
    /// servers, Machines, and Assets, which have none, at once). On a phone
    /// an item open fills the screen, and the sidebar button goes back up.
    pub fn at_list(self) -> bool {
        match self.view.get() {
            View::Chat => self.sidebar_open.get(),
            View::Agents => self.config_edit.with(Option::is_none),
            View::Settings => self.settings_section.get().is_none(),
            View::Memory => {
                self.memory_edit.with(Option::is_none) && self.memory_pending.with(Option::is_none)
            }
            View::Cron => self.cron_form.with(Option::is_none),
            View::Webhooks => {
                self.webhook_form.with(Option::is_none)
                    && self.webhook_selected.with(Option::is_none)
            }
            View::Assets => self.asset_page.with(Option::is_none),
            View::Skills | View::Mcp | View::Machines | View::Sharing => true,
        }
    }

    /// The layers open now that browser Back closes, innermost last.
    pub fn open_layers(self) -> Vec<Layer> {
        let mut layers = Vec::new();
        let view = self.view.get();
        if view != View::Chat {
            layers.push(Layer::View);
        }
        if view == View::Settings && self.narrow.get() && self.settings_section.get().is_some() {
            layers.push(Layer::Section);
        }
        if view == View::Webhooks && self.webhook_selected.with(Option::is_some) {
            layers.push(Layer::Item);
        }
        let editing = match view {
            View::Agents => self.config_edit.with(Option::is_some),
            View::Settings => {
                self.shown_section() == Some(SettingsSection::Configuration)
                    && self.config_edit.with(Option::is_some)
            }
            View::Memory => {
                self.memory_edit.with(Option::is_some) || self.memory_pending.with(Option::is_some)
            }
            View::Cron => self.cron_form.with(Option::is_some),
            View::Webhooks => self.webhook_form.with(Option::is_some),
            View::Assets => self.asset_page.with(Option::is_some),
            View::Chat | View::Skills | View::Mcp | View::Machines | View::Sharing => false,
        };
        if editing {
            layers.push(Layer::Editor);
        }
        if self.sidebar_open.get() {
            layers.push(Layer::Sidebar);
        }
        if self.panel.with(Option::is_some) {
            layers.push(Layer::Panel);
        }
        if self.transient.get() {
            layers.push(Layer::Transient);
        }
        layers
    }

    /// Keeps one history entry per open layer, until the returned listener
    /// is removed.
    pub fn follow_history(self) -> Option<browser::Listener> {
        Effect::new(move || {
            let open = self.open_layers();
            self.sync_history(&open);
        });
        browser::on_history_move(move |depth| {
            self.traversing.set_value(false);
            // Back past entries the page added closes their layers.
            while self.history.with_value(Vec::len) > depth {
                if let Some(layer) = self.history.try_update_value(Vec::pop).flatten() {
                    self.close_layer(layer);
                }
            }
            let open = untrack(|| self.open_layers());
            self.sync_history(&open);
        })
    }

    fn sync_history(self, open: &[Layer]) {
        if self.traversing.get_value() {
            return;
        }
        match self.history.with_value(|pushed| reconcile(pushed, open)) {
            HistoryStep::Keep => {}
            HistoryStep::Push(layers) => {
                for layer in layers {
                    self.history.update_value(|pushed| pushed.push(layer));
                    browser::push_history(self.history.with_value(Vec::len));
                }
            }
            HistoryStep::Back(steps) => {
                self.history.update_value(|pushed| {
                    pushed.truncate(pushed.len() - steps);
                });
                self.traversing.set_value(true);
                browser::history_back(steps);
            }
        }
    }

    fn close_layer(self, layer: Layer) {
        match layer {
            Layer::Transient => {
                self.transient.set(false);
                if let Some(close) = self
                    .transient_close
                    .try_update_value(Option::take)
                    .flatten()
                {
                    close.run(());
                }
            }
            Layer::View => self.open_view(View::Chat, None),
            Layer::Panel => self.close_panel(),
            Layer::Sidebar => self.sidebar_open.set(false),
            Layer::Section => self.settings_section.set(None),
            Layer::Item => {
                self.webhook_selected.set(None);
                self.webhook_event.set(None);
                self.webhook_revealed.set(None);
            }
            Layer::Editor => {
                self.config_edit.set(None);
                self.config_error.set(None);
                self.memory_edit.set(None);
                self.memory_pending.set(None);
                self.cron_form.set(None);
                self.webhook_form.set(None);
                self.asset_page.set(None);
            }
        }
    }

    /// A small overlay opened: Back closes it with `close` before anything
    /// under it.
    pub fn open_transient(self, close: Callback<()>) {
        self.transient_close.set_value(Some(close));
        self.transient.set(true);
    }

    /// Whether a small overlay (a row's menu, a context menu, the media
    /// viewer) is open over everything else.
    pub fn transient_open(self) -> bool {
        self.transient.get_untracked()
    }

    /// The small overlay closed by other means than Back.
    pub fn transient_closed(self) {
        self.transient_close.set_value(None);
        if self.transient.get_untracked() {
            self.transient.set(false);
        }
    }

    /// The back arrow of a phone's title bar: the same as browser Back.
    pub fn go_back(self) {
        if self.history.with_value(Vec::is_empty) {
            // No entry of the page's to go back to: close the layer here.
            if let Some(layer) = untrack(|| self.open_layers()).pop() {
                self.close_layer(layer);
            }
        } else {
            browser::history_back(1);
        }
    }
}
