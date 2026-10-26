//! Where the page is, independent of the browser: the rail's views, the
//! Settings sections, the history entries browser Back closes, and the
//! press that offers to delete a session row on a touch screen, and where
//! a row's ⋮ menu opens. The session's ⋮ menu is [`crate::commands`].

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
    /// The daemon's cron jobs (client protocol 1.15).
    Cron,
}

impl View {
    /// The rail's order.
    /// The rail's order: Settings comes after every other view.
    pub const ALL: [Self; 6] = [
        Self::Chat,
        Self::Agents,
        Self::Machines,
        Self::Memory,
        Self::Cron,
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
            Self::Cron => "Scheduled prompts",
        }
    }

    /// Whether the view has a list beside it that the sidebar button shows
    /// and hides: Chat's sessions, and Agents' agents. A desktop pins it
    /// beside the transcript (the agent open); a narrower window shows it
    /// as the page in their place.
    pub const fn has_sidebar(self) -> bool {
        matches!(self, Self::Chat | Self::Agents)
    }

    /// Where a phone shows the view's list, its first level, which a tap
    /// on its rail button shows before any of its items: Chat's sessions
    /// in its sidebar, shown as the page; the agents, the Settings
    /// sections, the memories, and the cron jobs as the page itself until
    /// one opens. Machines has no list.
    pub const fn phone_list(self) -> Option<PhoneList> {
        match self {
            Self::Chat => Some(PhoneList::Sidebar),
            Self::Agents | Self::Settings | Self::Memory | Self::Cron => Some(PhoneList::Page),
            Self::Machines => None,
        }
    }

    /// Whether a tap on the view's rail button opens its sidebar, on a
    /// `phone`: for a list the sidebar holds. Anywhere else the tap closes
    /// the sidebar.
    pub const fn rail_opens_sidebar(self, phone: bool) -> bool {
        phone && matches!(self.phone_list(), Some(PhoneList::Sidebar))
    }

    /// The `data-view` value of its rail button and page.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Agents => "agents",
            Self::Settings => "settings",
            Self::Machines => "machines",
            Self::Memory => "memory",
            Self::Cron => "cron",
        }
    }
}

/// Where a phone shows a view's list; see [`View::phone_list`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhoneList {
    /// In the sidebar, which fills the page beside the rail.
    Sidebar,
    /// As the page, until one of its items opens over it.
    Page,
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
    /// Push notifications: this browser's, and every device the daemon
    /// pushes to.
    Notifications,
    /// This browser: installing the page as an app, and what it keeps:
    /// the diagnostics filter and the token.
    Browser,
    /// The client's and the daemon's versions, the connection, and the
    /// credits.
    About,
}

impl SettingsSection {
    /// The order the section list shows.
    pub const ALL: [Self; 7] = [
        Self::Configuration,
        Self::Providers,
        Self::Mcp,
        Self::Plugins,
        Self::Notifications,
        Self::Browser,
        Self::About,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Configuration => "Configuration",
            Self::Providers => "Providers",
            Self::Mcp => "MCP servers",
            Self::Plugins => "Plugins",
            Self::Notifications => "Notifications",
            Self::Browser => "This browser",
            Self::About => "About",
        }
    }

    /// What the section holds, under its name in the list.
    pub const fn note(self) -> &'static str {
        match self {
            Self::Configuration => "the daemon's config.toml",
            Self::Providers => "connect or disconnect a model provider",
            Self::Mcp => "every runtime's MCP servers",
            Self::Plugins => "plugin status and restarts",
            Self::Notifications => "push notifications on this browser and your devices",
            Self::Browser => "installing the app, diagnostics shown, and forgetting this daemon",
            Self::About => "versions, the connection, links, and credits",
        }
    }

    /// The `data-section` value of its entry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Providers => "providers",
            Self::Mcp => "mcp",
            Self::Plugins => "plugins",
            Self::Notifications => "notifications",
            Self::Browser => "browser",
            Self::About => "about",
        }
    }
}

/// Something open that browser Back closes, innermost last.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layer {
    /// The sidebar shown as the page below desktop widths: the sessions in
    /// place of the transcript, or the agents in place of the agent open.
    Sidebar,
    /// A phone's Settings section, opened from the section list.
    Section,
    /// A document, memory, or cron job open in its editor.
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

    /// Whether the press fired as a long press, until the next one starts.
    pub fn fired(&self) -> bool {
        self.fired
    }

    /// The click the press ends with; `true` when it opens the session,
    /// which a long press never does.
    pub fn click(&mut self) -> bool {
        let opens = !self.fired;
        *self = Self::default();
        opens
    }
}

/// The gap between a row's ⋮ and the menu it opens, in CSS pixels.
pub const MENU_GAP: f64 = 4.0;

/// Where a row's menu stands in a wide window, in CSS pixels from the
/// viewport's edges: its right edge under the ⋮'s, and its top just
/// below the ⋮, or its bottom just above it when the menu would run past
/// the bottom of the screen there and there is more room above.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuPlace {
    /// From the viewport's top to the menu's, when it opens downwards.
    pub top: Option<f64>,
    /// From the viewport's bottom to the menu's, when it opens upwards.
    pub bottom: Option<f64>,
    /// From the viewport's right edge to the menu's.
    pub right: f64,
}

/// Places a menu `height` px tall for a ⋮ spanning `top` to `bottom` with
/// its right edge at `right`, in a viewport `width` by `viewport_height`.
pub fn menu_place(
    (top, bottom, right): (f64, f64, f64),
    (width, viewport_height): (f64, f64),
    height: f64,
) -> MenuPlace {
    let below = viewport_height - bottom - MENU_GAP;
    let above = top - MENU_GAP;
    let right = (width - right).max(0.0);
    if height <= below || below >= above {
        MenuPlace {
            top: Some(bottom + MENU_GAP),
            bottom: None,
            right,
        }
    } else {
        MenuPlace {
            top: None,
            bottom: Some(viewport_height - top + MENU_GAP),
            right,
        }
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
        use Layer::{Editor, Section, Sidebar};
        assert_eq!(reconcile(&[], &[]), HistoryStep::Keep);
        assert_eq!(reconcile(&[], &[Sidebar]), HistoryStep::Push(vec![Sidebar]));
        assert_eq!(
            reconcile(&[Section], &[Section, Editor]),
            HistoryStep::Push(vec![Editor])
        );
        assert_eq!(
            reconcile(&[Section, Editor], &[Section]),
            HistoryStep::Back(1)
        );
        assert_eq!(reconcile(&[Section, Editor], &[]), HistoryStep::Back(2));
        // Closing the sidebar by choosing a view that opens a section: back
        // first, the section's entry once the browser is there.
        assert_eq!(reconcile(&[Sidebar], &[Section]), HistoryStep::Back(1));
        assert_eq!(
            reconcile(&[Sidebar, Section], &[Sidebar, Section]),
            HistoryStep::Keep
        );
    }

    #[test]
    fn a_phone_rail_tap_shows_the_views_list_and_back_returns_to_it() {
        use Layer::{Editor, Section, Sidebar};
        // A tap on Chat opens a phone's sessions as the page, a tap on a
        // view whose list is its page closes them.
        assert!(View::Chat.rail_opens_sidebar(true));
        assert!(
            !View::Chat.rail_opens_sidebar(false),
            "a wider window opens on the transcript"
        );
        let after_tap = |view: View| -> Vec<Layer> {
            if view.rail_opens_sidebar(true) {
                vec![Sidebar]
            } else {
                Vec::new()
            }
        };
        assert_eq!(
            reconcile(&[Sidebar], &after_tap(View::Chat)),
            HistoryStep::Keep
        );
        for view in [
            View::Agents,
            View::Settings,
            View::Memory,
            View::Cron,
            View::Machines,
        ] {
            assert!(!view.rail_opens_sidebar(true), "{view:?}");
            assert_eq!(
                reconcile(&[Sidebar], &after_tap(view)),
                HistoryStep::Back(1)
            );
        }
        // Choosing an item of a page list opens a detail Back closes, back
        // to the list.
        for detail in [Section, Editor] {
            assert_eq!(reconcile(&[], &[detail]), HistoryStep::Push(vec![detail]));
            assert_eq!(reconcile(&[detail], &[]), HistoryStep::Back(1));
        }
        assert_eq!(View::Machines.phone_list(), None);
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

    #[test]
    fn a_long_press_stays_fired_after_the_finger_lifts_until_the_next_press() {
        let mut press = Press::default();
        press.start(10.0, 10.0);
        assert!(!press.fired());
        assert!(press.elapse());
        press.cancel();
        assert!(press.fired(), "the lift ends a long press");
        press.start(10.0, 10.0);
        assert!(!press.fired(), "a new press starts unfired");
    }

    #[test]
    fn a_row_menu_opens_below_its_button_unless_the_screen_ends_first() {
        // Room below: under the ⋮, right edges level.
        assert_eq!(
            menu_place((100.0, 144.0, 280.0), (1280.0, 800.0), 60.0),
            MenuPlace {
                top: Some(148.0),
                bottom: None,
                right: 1000.0,
            }
        );
        // Exactly enough room below still opens downwards.
        assert_eq!(
            menu_place((700.0, 744.0, 280.0), (1280.0, 800.0), 52.0).top,
            Some(748.0)
        );
        // Too little room below and more above: over the ⋮.
        assert_eq!(
            menu_place((700.0, 744.0, 280.0), (1280.0, 800.0), 53.0),
            MenuPlace {
                top: None,
                bottom: Some(104.0),
                right: 1000.0,
            }
        );
        // Too little room either way: wherever there is more.
        assert_eq!(
            menu_place((20.0, 64.0, 280.0), (1280.0, 100.0), 90.0).top,
            Some(68.0)
        );
        // A ⋮ past the right edge keeps the menu on the screen.
        assert_eq!(
            menu_place((100.0, 144.0, 1300.0), (1280.0, 800.0), 60.0).right,
            0.0
        );
    }
}
