//! Where the page is, independent of the browser: the rail's views, the
//! Settings sections, the history entries browser Back closes, and the
//! press that offers to delete a session row on a touch screen. The
//! session's ⋮ menu is [`crate::commands`].

/// A full view the rail switches to.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum View {
    /// The sessions and the transcript.
    #[default]
    Chat,
    /// The agent documents and their presets.
    Agents,
    /// `config.toml`, providers, MCP servers, plugins, and this browser's
    /// preferences.
    Settings,
    /// The daemon's machines and its runtimes.
    Machines,
    Memory,
}

impl View {
    /// The rail's order.
    /// The rail's order: Settings comes after every other view.
    pub const ALL: [Self; 5] = [
        Self::Chat,
        Self::Agents,
        Self::Machines,
        Self::Memory,
        Self::Settings,
    ];

    /// The view's name, as its rail button and title bar say it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Agents => "Agents",
            Self::Settings => "Settings",
            Self::Machines => "Machines & runtimes",
            Self::Memory => "Memory",
        }
    }

    /// The `data-view` value of its rail button and page.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agents => "agents",
            Self::Settings => "settings",
            Self::Machines => "machines",
            Self::Memory => "memory",
        }
    }
}

/// A part of the Settings view.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SettingsSection {
    /// `config.toml`.
    Configuration,
    /// The model providers (`/connect`).
    Providers,
    Mcp,
    Plugins,
    /// This browser: installing the page as an app, and what it keeps:
    /// the diagnostics filter and the token.
    Browser,
}

impl SettingsSection {
    /// The order the section list shows.
    pub const ALL: [Self; 5] = [
        Self::Configuration,
        Self::Providers,
        Self::Mcp,
        Self::Plugins,
        Self::Browser,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Configuration => "Configuration",
            Self::Providers => "Providers",
            Self::Mcp => "MCP servers",
            Self::Plugins => "Plugins",
            Self::Browser => "This browser",
        }
    }

    /// What the section holds, under its name in the list.
    pub const fn note(self) -> &'static str {
        match self {
            Self::Configuration => "the daemon's config.toml",
            Self::Providers => "connect or disconnect a model provider",
            Self::Mcp => "every runtime's MCP servers",
            Self::Plugins => "plugin status and restarts",
            Self::Browser => "installing the app, diagnostics shown, and forgetting this daemon",
        }
    }

    /// The `data-section` value of its entry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Providers => "providers",
            Self::Mcp => "mcp",
            Self::Plugins => "plugins",
            Self::Browser => "browser",
        }
    }
}

/// Something open that browser Back closes, innermost last.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layer {
    /// The phone's drawer, or the sessions sliding over a tablet's
    /// transcript.
    Drawer,
    /// A phone's Settings section, opened from the section list.
    Section,
    /// A document or memory open in its editor.
    Editor,
}

/// What the page does to the browser's history so that it holds one entry
/// per open layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryStep {
    /// Nothing: the history matches.
    Keep,
    /// Push an entry for each of these, in order.
    Push(Vec<Layer>),
    /// Go back this many entries: their layers closed by other means than
    /// Back. Whatever must be pushed after is reconciled once the browser
    /// arrives.
    Back(usize),
}

/// The step from the layers the history holds entries for (`pushed`) to
/// the layers open now (`open`).
pub fn reconcile(pushed: &[Layer], open: &[Layer]) -> HistoryStep {
    let common = pushed
        .iter()
        .zip(open)
        .take_while(|(pushed, open)| pushed == open)
        .count();
    if common < pushed.len() {
        HistoryStep::Back(pushed.len() - common)
    } else if common < open.len() {
        HistoryStep::Push(open[common..].to_vec())
    } else {
        HistoryStep::Keep
    }
}

/// How far a finger may move during a long press before it counts as a
/// scroll, in CSS pixels.
pub const PRESS_SLOP: f64 = 10.0;

/// A press on a session row: held long enough without moving, it reveals
/// the row's delete action instead of opening the session.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Press {
    start: Option<(f64, f64)>,
    /// The long press fired: the click that follows must not open.
    fired: bool,
}

impl Press {
    /// A finger went down at `x`, `y`.
    pub fn start(&mut self, x: f64, y: f64) {
        *self = Self {
            start: Some((x, y)),
            fired: false,
        };
    }

    /// The finger moved to `x`, `y`; `true` when that cancels the press,
    /// as a scroll does.
    pub fn moved(&mut self, x: f64, y: f64) -> bool {
        let Some((start_x, start_y)) = self.start else {
            return false;
        };
        if (x - start_x).hypot(y - start_y) > PRESS_SLOP {
            self.start = None;
            return true;
        }
        false
    }

    /// The finger lifted or the browser took the gesture over.
    pub fn cancel(&mut self) {
        self.start = None;
    }

    /// The hold timer ran out; `true` when the press is still down, so it
    /// is a long press.
    pub fn elapse(&mut self) -> bool {
        if self.start.take().is_some() {
            self.fired = true;
            return true;
        }
        false
    }

    /// The click the press ends with; `true` when it opens the session,
    /// which a long press never does.
    pub fn click(&mut self) -> bool {
        let opens = !self.fired;
        *self = Self::default();
        opens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_is_the_last_view_on_the_rail() {
        assert_eq!(View::ALL.last(), Some(&View::Settings));
    }

    #[test]
    fn views_and_sections_have_distinct_ids() {
        let mut views = View::ALL.map(View::id).to_vec();
        views.sort_unstable();
        views.dedup();
        assert_eq!(views.len(), View::ALL.len());
        let mut sections = SettingsSection::ALL.map(SettingsSection::id).to_vec();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(sections.len(), SettingsSection::ALL.len());
        assert_eq!(View::default(), View::Chat);
    }

    #[test]
    fn the_history_gains_an_entry_per_opened_layer_and_gives_back_closed_ones() {
        use Layer::{Drawer, Editor, Section};
        assert_eq!(reconcile(&[], &[]), HistoryStep::Keep);
        assert_eq!(reconcile(&[], &[Drawer]), HistoryStep::Push(vec![Drawer]));
        assert_eq!(
            reconcile(&[Section], &[Section, Editor]),
            HistoryStep::Push(vec![Editor])
        );
        assert_eq!(
            reconcile(&[Section, Editor], &[Section]),
            HistoryStep::Back(1)
        );
        assert_eq!(reconcile(&[Section, Editor], &[]), HistoryStep::Back(2));
        // Closing the drawer by choosing a view that opens a section: back
        // first, the section's entry once the browser is there.
        assert_eq!(reconcile(&[Drawer], &[Section]), HistoryStep::Back(1));
        assert_eq!(
            reconcile(&[Drawer, Section], &[Drawer, Section]),
            HistoryStep::Keep
        );
    }

    #[test]
    fn a_long_press_reveals_instead_of_opening_and_a_move_cancels_it() {
        let mut press = Press::default();
        press.start(100.0, 200.0);
        assert!(!press.moved(104.0, 203.0), "a tremble is not a scroll");
        assert!(press.elapse());
        assert!(!press.click(), "a long press never opens the session");
        assert!(press.click(), "the next tap opens it");

        press.start(100.0, 200.0);
        assert!(press.moved(100.0, 200.0 + PRESS_SLOP + 1.0));
        assert!(!press.elapse(), "a scroll cancels the long press");
        assert!(press.click());

        press.start(0.0, 0.0);
        press.cancel();
        assert!(!press.elapse(), "lifting early is a tap");
        assert!(press.click());
    }
}
