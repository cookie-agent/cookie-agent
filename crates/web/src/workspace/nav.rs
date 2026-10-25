//! Moving around the page: the rail's views, the Settings sections, the
//! sessions sidebar, and the history entries that let browser Back close
//! what is open, one layer at a time.

use leptos::prelude::*;

use super::{Panel, Workspace};
use crate::{
    browser,
    connect::SIDEBAR_KEY,
    layout::{HistoryStep, Layer, SettingsSection, View, reconcile},
};

impl Workspace {
    /// Shows `view`, reading what it lists; `section` picks the Settings
    /// section. It closes any sheet and the drawer.
    pub fn open_view(self, view: View, section: Option<SettingsSection>) {
        self.panel.set(None);
        self.sidebar_open.set(false);
        match view {
            View::Chat => {}
            View::Agents => self.open_config(),
            View::Settings => self.open_section(section),
            View::Machines => self.poll_runtimes(),
            View::Memory => self.open_memory(),
            View::Cron => self.open_crons(),
        }
        self.view.set(view);
    }

    /// A tap on `view`'s rail button: on a phone, a view with a list shows
    /// the list first (see [`View::phone_list`]); choosing one of its
    /// items opens it.
    pub fn open_from_rail(self, view: View) {
        self.open_view(view, None);
        if view.rail_keeps_drawer(self.narrow.get_untracked()) {
            self.sidebar_open.set(true);
        }
    }

    /// Opens a Settings section (`None`: the list, on a phone), reading
    /// what it lists.
    pub fn open_section(self, section: Option<SettingsSection>) {
        match section {
            None | Some(SettingsSection::Configuration) => self.open_config(),
            Some(SettingsSection::Mcp) => self.poll_mcp(),
            Some(SettingsSection::Plugins) => self.poll_plugins(),
            Some(SettingsSection::Notifications) => self.poll_notifications(),
            Some(SettingsSection::Providers | SettingsSection::Browser) => {}
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
                Some(SettingsSection::Mcp) => self.poll_mcp(),
                Some(SettingsSection::Plugins) => self.poll_plugins(),
                Some(SettingsSection::Notifications) => self.poll_notifications(),
                Some(SettingsSection::Providers | SettingsSection::Browser) => {}
            },
            View::Memory => self.poll_memory(),
            View::Chat | View::Machines | View::Cron => {}
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

    /// The sidebar button: a desktop pins or hides the sidebar of Chat and
    /// Agents, remembered in the browser; a narrower window slides the
    /// sessions (the agents, in Agents) over.
    pub fn toggle_sidebar(self) {
        if browser::matches(browser::DESKTOP_QUERY) && self.view.get_untracked().has_sidebar() {
            let shown = !self.sidebar_pinned.get_untracked();
            browser::store(SIDEBAR_KEY, if shown { "shown" } else { "hidden" });
            self.sidebar_pinned.set(shown);
        } else {
            self.sidebar_open.update(|open| *open = !*open);
        }
    }

    /// The layers open now that browser Back closes, innermost last.
    pub fn open_layers(self) -> Vec<Layer> {
        let mut layers = Vec::new();
        let view = self.view.get();
        if view == View::Settings && self.narrow.get() && self.settings_section.get().is_some() {
            layers.push(Layer::Section);
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
            View::Chat | View::Machines => false,
        };
        if editing {
            layers.push(Layer::Editor);
        }
        if self.sidebar_open.get() {
            layers.push(Layer::Drawer);
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
            Layer::Drawer => self.sidebar_open.set(false),
            Layer::Section => self.settings_section.set(None),
            Layer::Editor => {
                self.config_edit.set(None);
                self.config_error.set(None);
                self.memory_edit.set(None);
                self.memory_pending.set(None);
                self.cron_form.set(None);
            }
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
