//! The new-session runtime cards, independent of the browser: one card per
//! runtime capability that takes a workspace, plus `global`, grouped by the
//! machine each runtime runs on; which cards a new session selects and at
//! which workspaces; the defaults, the workspace suggestions, the note
//! about tool-name prefixes, and what the browser remembers.
//!
//! The selection keeps the text the user typed: the daemon and the runtime
//! resolve it (`~/…` included). Opening a workspace answers its canonical
//! path, which the session is created with.

use std::collections::BTreeMap;

use cookie_agent_client_state::paths::RuntimeHomes;
use cookie_agent_protocol::{
    CapabilityName, MachineName, RuntimeAttachRequest, RuntimeAttachment, RuntimeId,
    RuntimeLifetime, RuntimesListResult, SessionMeta,
};
use serde_json::{Value, json};

/// The `localStorage` key of the last selection a session started with.
pub const SELECTION_KEY: &str = "cookie.new_session";

/// The `localStorage` key of the workspaces recently opened on
/// `runtime_id`, newest first.
pub fn recent_key(runtime_id: &str) -> String {
    format!("cookie.recent_workspaces:{runtime_id}")
}

/// The `localStorage` key of whether `machine`'s group is open.
pub fn group_key(machine: &str) -> String {
    format!("cookie.machine_open:{machine}")
}

/// How many recent workspaces the browser keeps per runtime.
pub const RECENT_LIMIT: usize = 8;

/// How many suggestions a workspace field offers.
pub const SUGGESTION_LIMIT: usize = 8;

/// Whether a card's runtime can start a session now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CardState {
    /// Connected (or built in).
    Ready,
    /// Not running, but the daemon starts it when a session opens it.
    StartsOnUse,
    /// A runtime that dials in and is not connected: it cannot be opened.
    Offline,
}

impl CardState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::StartsOnUse => "starts on use",
            Self::Offline => "offline",
        }
    }
}

/// One card: a runtime capability a session can attach.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Card {
    pub runtime_id: RuntimeId,
    /// `None` for a runtime that has not said what it offers yet: attaching
    /// takes its only capability.
    pub capability: Option<CapabilityName>,
    pub takes_workspace: bool,
    pub state: CardState,
    pub machine: Option<MachineName>,
    pub description: String,
    /// Why the runtime last failed to connect.
    pub message: Option<String>,
}

impl Card {
    /// What identifies the card in the selection and the page.
    pub fn key(&self) -> String {
        card_key(
            self.runtime_id.as_str(),
            self.capability.as_ref().map(CapabilityName::as_str),
        )
    }

    pub fn is_global(&self) -> bool {
        self.runtime_id.as_str() == RuntimeId::GLOBAL
    }

    /// `local/coding`, or the runtime alone before it said what it offers.
    pub fn label(&self) -> String {
        match &self.capability {
            Some(capability) => format!("{}/{capability}", self.runtime_id),
            None => self.runtime_id.to_string(),
        }
    }
}

fn card_key(runtime_id: &str, capability: Option<&str>) -> String {
    match capability {
        Some(capability) => format!("{runtime_id}/{capability}"),
        None => runtime_id.to_owned(),
    }
}

/// The cards `listing` offers: `global` first, then `local`, then every
/// other runtime's capabilities that take a workspace by runtime ID, and a
/// runtime that has not said what it offers as one card that asks for a
/// workspace.
pub fn cards(listing: &RuntimesListResult) -> Vec<Card> {
    let mut cards = Vec::new();
    for runtime in &listing.runtimes {
        let state = if runtime.connected || runtime.runtime_id.as_str() == RuntimeId::GLOBAL {
            CardState::Ready
        } else if runtime.lifetime == RuntimeLifetime::DialIn {
            CardState::Offline
        } else {
            CardState::StartsOnUse
        };
        let card =
            |capability: Option<CapabilityName>, takes_workspace, description: String| Card {
                runtime_id: runtime.runtime_id.clone(),
                capability,
                takes_workspace,
                state,
                machine: runtime.machine.clone(),
                description,
                message: (!runtime.connected)
                    .then(|| runtime.message.clone())
                    .flatten(),
            };
        if runtime.runtime_id.as_str() == RuntimeId::GLOBAL {
            cards.push(card(
                Some(CapabilityName::global()),
                false,
                "The daemon's own tools, MCP servers, and memory".into(),
            ));
        } else if runtime.capabilities.is_empty() {
            // It has not said what it offers yet: say how it comes to run.
            let how = match runtime.lifetime {
                RuntimeLifetime::DialIn => "Dials in · not connected since the daemon started",
                RuntimeLifetime::Builtin => "Built in",
                RuntimeLifetime::Spawned => {
                    "Template runtime · the daemon starts it when a session needs it"
                }
            };
            cards.push(card(None, true, how.to_owned()));
        } else {
            for capability in runtime.capabilities.iter().filter(|c| c.takes_workspace) {
                cards.push(card(
                    Some(capability.name.clone()),
                    true,
                    capability.description.clone(),
                ));
            }
        }
    }
    cards.sort_by_key(|card| {
        (
            !card.is_global(),
            card.runtime_id.as_str() != RuntimeId::LOCAL,
            card.runtime_id.clone(),
            card.capability.clone(),
        )
    });
    cards
}

/// The machine this daemon runs on: `local`'s, else `global`'s.
pub fn daemon_machine(listing: &RuntimesListResult) -> Option<MachineName> {
    let machine_of = |id: &str| {
        listing
            .runtimes
            .iter()
            .find(|runtime| runtime.runtime_id.as_str() == id)
            .and_then(|runtime| runtime.machine.clone())
    };
    machine_of(RuntimeId::LOCAL).or_else(|| machine_of(RuntimeId::GLOBAL))
}

/// The cards of one machine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Group {
    /// `None` for runtimes whose machine the daemon cannot tell.
    pub machine: Option<MachineName>,
    /// This daemon's machine, which also holds `global`.
    pub daemon: bool,
    pub cards: Vec<Card>,
}

impl Group {
    /// The group's name and its key in the browser's storage.
    pub fn name(&self) -> String {
        self.machine
            .as_ref()
            .map_or_else(|| "other machines".to_owned(), ToString::to_string)
    }

    /// "yunhao-labpc · 2 runtimes · 1 ready · 1 selected".
    pub fn summary(&self, selection: &Selection) -> String {
        let mut runtimes = self
            .cards
            .iter()
            .map(|card| card.runtime_id.as_str())
            .collect::<Vec<_>>();
        runtimes.dedup();
        let ready = self
            .cards
            .iter()
            .filter(|card| card.state == CardState::Ready)
            .map(|card| card.runtime_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let selected = self
            .cards
            .iter()
            .filter(|card| selection.selected(card))
            .count();
        format!(
            "{} · {} · {ready} ready · {selected} selected",
            self.name(),
            plural(runtimes.len(), "runtime"),
        )
    }

    /// Whether the group shows its cards: as the browser remembers it, else
    /// open for this daemon's machine and for a machine with a selected card.
    pub fn open(&self, stored: Option<&str>, selection: &Selection) -> bool {
        match stored {
            Some("open") => true,
            Some("closed") => false,
            _ => self.daemon || self.cards.iter().any(|card| selection.selected(card)),
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `cards` grouped by machine: this daemon's machine first (with `global`),
/// then the others by name, and runtimes of no known machine last.
pub fn groups(cards: &[Card], daemon: Option<&MachineName>) -> Vec<Group> {
    let mut by_machine = BTreeMap::<Option<MachineName>, Vec<Card>>::new();
    for card in cards {
        let machine = if card.is_global() {
            daemon.cloned().or_else(|| card.machine.clone())
        } else {
            card.machine.clone()
        };
        by_machine.entry(machine).or_default().push(card.clone());
    }
    let mut groups = by_machine
        .into_iter()
        .map(|(machine, cards)| Group {
            daemon: machine.is_some() && machine.as_ref() == daemon,
            machine,
            cards,
        })
        .collect::<Vec<_>>();
    // A daemon whose machine is unknown keeps its built-in runtimes first.
    let holds_global = |group: &Group| group.cards.iter().any(Card::is_global);
    groups.sort_by_key(|group| {
        (
            !(group.daemon || holds_global(group)),
            group.machine.is_none(),
        )
    });
    groups
}

/// One selected card and the workspaces it opens, in field order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pick {
    pub runtime_id: String,
    pub capability: Option<String>,
    /// One entry per workspace field (empty while not filled in); none for
    /// a card that takes no workspace.
    pub workspaces: Vec<String>,
}

impl Pick {
    fn key(&self) -> String {
        card_key(&self.runtime_id, self.capability.as_deref())
    }

    /// Whether the pick is of `card`. A runtime that has not said what it
    /// offers has one card without a capability, which becomes its
    /// capability's card once it connected: either matches the other.
    fn is_of(&self, card: &Card) -> bool {
        self.runtime_id == card.runtime_id.as_str()
            && match (&self.capability, &card.capability) {
                (Some(picked), Some(offered)) => picked == offered.as_str(),
                _ => true,
            }
    }
}

/// What a new session starts with: `global` or not, and the picked cards.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    pub global: bool,
    pub picks: Vec<Pick>,
}

impl Default for Selection {
    /// The first selection ever: `global`, and `local`'s `coding` with an
    /// empty workspace field to fill in.
    fn default() -> Self {
        Self {
            global: true,
            picks: vec![Pick {
                runtime_id: RuntimeId::LOCAL.to_owned(),
                capability: Some(CapabilityName::CODING.to_owned()),
                workspaces: vec![String::new()],
            }],
        }
    }
}

impl Selection {
    /// The selection of a session attached as `attachments`, its
    /// workspaces shown under their runtime's home, as the fields show them.
    pub fn of_session(attachments: &[RuntimeAttachment], homes: &RuntimeHomes) -> Self {
        let mut selection = Self {
            global: false,
            picks: Vec::new(),
        };
        for attachment in attachments {
            if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
                selection.global = true;
                continue;
            }
            let Some(workspace) = &attachment.workspace else {
                continue;
            };
            let shown = homes.display(&attachment.runtime_id, workspace);
            let key = card_key(
                attachment.runtime_id.as_str(),
                Some(attachment.capability.as_str()),
            );
            match selection.picks.iter_mut().find(|pick| pick.key() == key) {
                Some(pick) => pick.workspaces.push(shown),
                None => selection.picks.push(Pick {
                    runtime_id: attachment.runtime_id.to_string(),
                    capability: Some(attachment.capability.to_string()),
                    workspaces: vec![shown],
                }),
            }
        }
        selection
    }

    /// The selection the browser kept as `stored` ([`Self::to_storage`]);
    /// `None` for anything else.
    pub fn from_storage(stored: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(stored).ok()?;
        let picks = value
            .get("picks")?
            .as_array()?
            .iter()
            .map(|pick| {
                Some(Pick {
                    runtime_id: pick.get("runtime")?.as_str()?.to_owned(),
                    capability: pick
                        .get("capability")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    workspaces: pick
                        .get("workspaces")?
                        .as_array()?
                        .iter()
                        .map(|workspace| workspace.as_str().map(str::to_owned))
                        .collect::<Option<_>>()?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            global: value.get("global")?.as_bool()?,
            picks,
        })
    }

    pub fn to_storage(&self) -> String {
        json!({
            "global": self.global,
            "picks": self.picks.iter().map(|pick| json!({
                "runtime": pick.runtime_id,
                "capability": pick.capability,
                "workspaces": pick.workspaces,
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn pick(&self, card: &Card) -> Option<&Pick> {
        self.picks.iter().find(|pick| pick.is_of(card))
    }

    fn pick_mut(&mut self, card: &Card) -> Option<&mut Pick> {
        self.picks.iter_mut().find(|pick| pick.is_of(card))
    }

    pub fn selected(&self, card: &Card) -> bool {
        if card.is_global() {
            self.global
        } else {
            self.pick(card).is_some()
        }
    }

    /// Selects or deselects `card`; an offline card cannot be selected.
    /// Returns whether the selection changed.
    pub fn toggle(&mut self, card: &Card) -> bool {
        if card.is_global() {
            self.global = !self.global;
            return true;
        }
        if self.selected(card) {
            self.picks.retain(|pick| !pick.is_of(card));
            return true;
        }
        if card.state == CardState::Offline {
            return false;
        }
        self.picks.push(Pick {
            runtime_id: card.runtime_id.to_string(),
            capability: card.capability.as_ref().map(ToString::to_string),
            workspaces: if card.takes_workspace {
                vec![String::new()]
            } else {
                Vec::new()
            },
        });
        true
    }

    /// The workspace fields of `card`, in order.
    pub fn workspaces(&self, card: &Card) -> Vec<String> {
        self.pick(card)
            .map(|pick| pick.workspaces.clone())
            .unwrap_or_default()
    }

    pub fn set_workspace(&mut self, card: &Card, index: usize, text: String) {
        if let Some(field) = self
            .pick_mut(card)
            .and_then(|pick| pick.workspaces.get_mut(index))
        {
            *field = text;
        }
    }

    /// "+ another workspace": the same runtime at a second directory.
    pub fn add_workspace(&mut self, card: &Card) {
        if let Some(pick) = self.pick_mut(card) {
            pick.workspaces.push(String::new());
        }
    }

    /// Removes a workspace field; the last one stays.
    pub fn remove_workspace(&mut self, card: &Card, index: usize) {
        if let Some(pick) = self.pick_mut(card)
            && pick.workspaces.len() > 1
            && index < pick.workspaces.len()
        {
            pick.workspaces.remove(index);
        }
    }

    /// Why Start is not offered, else `None`: a selected card with an empty
    /// workspace field, or one gone offline.
    pub fn blocker(&self, cards: &[Card]) -> Option<String> {
        for card in cards
            .iter()
            .filter(|card| !card.is_global() && self.selected(card))
        {
            if card.state == CardState::Offline {
                return Some(format!("{} is offline", card.label()));
            }
            if card.takes_workspace
                && self
                    .workspaces(card)
                    .iter()
                    .any(|workspace| workspace.trim().is_empty())
            {
                return Some(format!("enter a workspace for {}", card.label()));
            }
        }
        None
    }

    /// What the session opens, in card order: each selected card at each
    /// of its workspaces (as typed), and whether it attaches `global`.
    pub fn requests(&self, cards: &[Card]) -> (bool, Vec<RuntimeAttachRequest>) {
        let (global, requests) = self.keyed_requests(cards);
        (
            global,
            requests
                .into_iter()
                .map(|(_, _, request)| request)
                .collect(),
        )
    }

    /// [`Self::requests`], each with its card's key and its field's index,
    /// where a refusal shows.
    pub fn keyed_requests(
        &self,
        cards: &[Card],
    ) -> (bool, Vec<(String, usize, RuntimeAttachRequest)>) {
        let mut requests = Vec::new();
        for card in cards
            .iter()
            .filter(|card| !card.is_global() && self.selected(card))
        {
            let request = |workspace: Option<String>| RuntimeAttachRequest {
                runtime_id: card.runtime_id.clone(),
                capability: card.capability.clone(),
                workspace,
            };
            if card.takes_workspace {
                for (index, workspace) in self.workspaces(card).into_iter().enumerate() {
                    requests.push((
                        card.key(),
                        index,
                        request(Some(workspace.trim().to_owned())),
                    ));
                }
            } else {
                requests.push((card.key(), 0, request(None)));
            }
        }
        (self.global, requests)
    }

    /// The note that tools get prefixes, when two or more workspaces are
    /// selected: each workspace attachment then publishes its tools as
    /// `<attachment>_<tool>`, the attachment named after its runtime and
    /// the workspace's last component.
    pub fn prefix_note(&self, cards: &[Card]) -> Option<String> {
        let (_, requests) = self.requests(cards);
        let workspaces = requests
            .iter()
            .filter_map(|request| {
                Some((request.runtime_id.as_str(), request.workspace.as_deref()?))
            })
            .collect::<Vec<_>>();
        if workspaces.len() < 2 {
            return None;
        }
        let (runtime_id, workspace) = workspaces[0];
        Some(format!(
            "With {} workspaces, their tools get prefixes: bash becomes {}_bash.",
            workspaces.len(),
            attachment_name(runtime_id, workspace)
        ))
    }
}

/// The attachment name a coding runtime gives a workspace, as
/// `cookie_agent_runtime_api::coding_attachment_name` makes it:
/// `<runtime>_<last component>`, lowercased, every other character `_`,
/// runs of `_` collapsed, at most 32 bytes, no trailing `_`.
pub fn attachment_name(runtime_id: &str, workspace: &str) -> String {
    let trimmed = workspace.trim().trim_end_matches(['/', '\\']);
    let basename = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && *name != "~")
        .unwrap_or_default();
    let mut name = String::new();
    for character in format!("{runtime_id}_{basename}").chars() {
        let character = character.to_ascii_lowercase();
        let character = if character.is_ascii_lowercase() || character.is_ascii_digit() {
            character
        } else {
            '_'
        };
        if character == '_' && name.ends_with('_') {
            continue;
        }
        if name.len() == 32 {
            break;
        }
        name.push(character);
    }
    name.truncate(name.trim_end_matches('_').len());
    name
}

/// The workspaces a field for `runtime_id` suggests, shown under the
/// runtime's `home`: the ones this browser opened there lately (`recent`,
/// newest first), then the workspaces sessions attach there, most recently
/// active first; each once.
pub fn suggestions(
    runtime_id: &str,
    recent: &[String],
    sessions: &[SessionMeta],
    home: Option<&str>,
) -> Vec<String> {
    let mut sessions = sessions.iter().collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity));
    let attached = sessions.into_iter().flat_map(|session| {
        session
            .attachments
            .iter()
            .filter(|attachment| attachment.runtime_id.as_str() == runtime_id)
            .filter_map(|attachment| attachment.workspace.clone())
    });
    let mut shown = Vec::new();
    for workspace in recent.iter().cloned().chain(attached) {
        let display = cookie_agent_client_state::paths::display_path(&workspace, home);
        if !shown.contains(&display) {
            shown.push(display);
        }
        if shown.len() == SUGGESTION_LIMIT {
            break;
        }
    }
    shown
}

/// `recent` with `workspace` first, once, kept to [`RECENT_LIMIT`].
pub fn remember(recent: &[String], workspace: &str) -> Vec<String> {
    std::iter::once(workspace.to_owned())
        .chain(recent.iter().filter(|kept| *kept != workspace).cloned())
        .take(RECENT_LIMIT)
        .collect()
}

/// A stored list of recent workspaces; empty for anything else.
pub fn recent_from_storage(stored: Option<&str>) -> Vec<String> {
    stored
        .and_then(|stored| serde_json::from_str::<Vec<String>>(stored).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{RuntimeCapabilityInfo, RuntimeKind, RuntimeListing};

    use super::*;

    fn listing(runtimes: Vec<RuntimeListing>) -> RuntimesListResult {
        RuntimesListResult {
            runtimes,
            instances: Vec::new(),
        }
    }

    fn runtime(
        id: &str,
        lifetime: RuntimeLifetime,
        connected: bool,
        machine: Option<&str>,
        capabilities: &[(&str, bool)],
    ) -> RuntimeListing {
        RuntimeListing {
            runtime_id: RuntimeId::new(id).unwrap(),
            lifetime,
            connected,
            capabilities: capabilities
                .iter()
                .map(|(name, takes_workspace)| RuntimeCapabilityInfo {
                    name: CapabilityName::new(*name).unwrap(),
                    kind: RuntimeKind::new(*name).unwrap(),
                    takes_workspace: *takes_workspace,
                    description: String::new(),
                })
                .collect(),
            platform: None,
            machine: machine.map(|machine| MachineName::new(machine).unwrap()),
            home: None,
            message: (!connected).then(|| "not running".to_owned()),
        }
    }

    fn sample() -> RuntimesListResult {
        listing(vec![
            runtime(
                "local",
                RuntimeLifetime::Builtin,
                true,
                Some("devbox"),
                &[("coding", true)],
            ),
            runtime(
                "global",
                RuntimeLifetime::Builtin,
                true,
                Some("devbox"),
                &[("global", false)],
            ),
            runtime("box", RuntimeLifetime::Spawned, false, None, &[]),
            runtime(
                "labpc",
                RuntimeLifetime::DialIn,
                false,
                Some("labpc"),
                &[("coding", true), ("search", false)],
            ),
            runtime(
                "gpu",
                RuntimeLifetime::DialIn,
                true,
                Some("labpc"),
                &[("coding", true)],
            ),
        ])
    }

    fn card<'a>(cards: &'a [Card], key: &str) -> &'a Card {
        cards.iter().find(|card| card.key() == key).unwrap()
    }

    #[test]
    fn cards_are_global_and_the_capabilities_that_take_a_workspace() {
        let cards = cards(&sample());
        let keys = cards.iter().map(Card::key).collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "global/global",
                "local/coding",
                "box",
                "gpu/coding",
                "labpc/coding"
            ]
        );
        assert_eq!(card(&cards, "global/global").state, CardState::Ready);
        assert!(!card(&cards, "global/global").takes_workspace);
        assert_eq!(card(&cards, "local/coding").state, CardState::Ready);
        // A template the daemon starts, which has not said what it offers.
        let template = card(&cards, "box");
        assert_eq!(template.state, CardState::StartsOnUse);
        assert!(template.takes_workspace && template.capability.is_none());
        assert_eq!(
            template.description,
            "Template runtime · the daemon starts it when a session needs it"
        );
        assert_eq!(template.message.as_deref(), Some("not running"));
        assert_eq!(card(&cards, "labpc/coding").state, CardState::Offline);
        assert_eq!(card(&cards, "gpu/coding").state, CardState::Ready);
    }

    #[test]
    fn groups_put_this_daemons_machine_first_and_unknown_machines_last() {
        let listing = sample();
        let cards = cards(&listing);
        let daemon = daemon_machine(&listing);
        assert_eq!(daemon.as_ref().map(MachineName::as_str), Some("devbox"));
        let groups = groups(&cards, daemon.as_ref());
        let names = groups.iter().map(Group::name).collect::<Vec<_>>();
        assert_eq!(names, ["devbox", "labpc", "other machines"]);
        assert!(groups[0].daemon && !groups[1].daemon);
        assert_eq!(groups[0].cards[0].key(), "global/global");
        let selection = Selection::default();
        assert_eq!(
            groups[0].summary(&selection),
            "devbox · 2 runtimes · 2 ready · 2 selected"
        );
        assert_eq!(
            groups[1].summary(&selection),
            "labpc · 2 runtimes · 1 ready · 0 selected"
        );
        // Open by default: this daemon's machine, and one with a selected
        // card; the browser's choice wins.
        assert!(groups[0].open(None, &selection));
        assert!(!groups[1].open(None, &selection));
        assert!(groups[1].open(Some("open"), &selection));
        assert!(!groups[0].open(Some("closed"), &selection));
        let mut picked = selection.clone();
        picked.toggle(card(&cards, "gpu/coding"));
        assert!(groups[1].open(None, &picked));
    }

    #[test]
    fn selecting_cards_builds_the_requests_in_card_order() {
        let cards = cards(&sample());
        let mut selection = Selection {
            global: true,
            picks: Vec::new(),
        };
        // Clicked in another order than the cards show.
        assert!(selection.toggle(card(&cards, "gpu/coding")));
        assert!(selection.toggle(card(&cards, "local/coding")));
        assert_eq!(
            selection.blocker(&cards).as_deref(),
            Some("enter a workspace for local/coding")
        );
        selection.set_workspace(card(&cards, "local/coding"), 0, " ~/web ".into());
        selection.set_workspace(card(&cards, "gpu/coding"), 0, "/srv/api".into());
        // The same runtime at a second directory.
        selection.add_workspace(card(&cards, "local/coding"));
        assert!(selection.blocker(&cards).is_some());
        selection.set_workspace(card(&cards, "local/coding"), 1, "~/docs".into());
        assert_eq!(selection.blocker(&cards), None);
        let (global, requests) = selection.requests(&cards);
        assert!(global);
        let shown = requests
            .iter()
            .map(|request| {
                format!(
                    "{}:{}",
                    request.runtime_id,
                    request.workspace.as_deref().unwrap()
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(shown, ["local:~/web", "local:~/docs", "gpu:/srv/api"]);
        assert_eq!(
            requests[0].capability.as_ref().map(CapabilityName::as_str),
            Some("coding")
        );
        // Another field goes; the last one stays, as typed.
        selection.remove_workspace(card(&cards, "local/coding"), 1);
        selection.remove_workspace(card(&cards, "local/coding"), 0);
        assert_eq!(
            selection.workspaces(card(&cards, "local/coding")),
            [" ~/web "]
        );
    }

    #[test]
    fn a_pick_follows_its_runtime_once_it_says_what_it_offers() {
        let listing = sample();
        let before = cards(&listing);
        let mut selection = Selection::default();
        selection.toggle(card(&before, "box"));
        selection.set_workspace(card(&before, "box"), 0, "/srv/box".into());
        // `box` connected and offers `coding`: its card is `box/coding`.
        let mut connected = listing.clone();
        connected.runtimes[2].connected = true;
        connected.runtimes[2].capabilities = sample().runtimes[0].capabilities.clone();
        let after = cards(&connected);
        let boxed = card(&after, "box/coding");
        assert!(selection.selected(boxed));
        assert_eq!(selection.workspaces(boxed), ["/srv/box"]);
        let (_, requests) = selection.requests(&after);
        assert_eq!(
            requests[1].capability.as_ref().map(CapabilityName::as_str),
            Some("coding")
        );
        selection.toggle(boxed);
        assert!(!selection.selected(card(&before, "box")));
    }

    #[test]
    fn global_can_be_deselected_and_offline_cards_cannot_be_selected() {
        let cards = cards(&sample());
        let mut selection = Selection::default();
        assert!(selection.selected(card(&cards, "global/global")));
        selection.toggle(card(&cards, "global/global"));
        assert!(!selection.selected(card(&cards, "global/global")));
        assert!(!selection.toggle(card(&cards, "labpc/coding")));
        assert!(!selection.selected(card(&cards, "labpc/coding")));
        // No coding card: a session with no workspace.
        selection.toggle(card(&cards, "local/coding"));
        assert_eq!(selection.requests(&cards), (false, Vec::new()));
        assert_eq!(selection.blocker(&cards), None);
        // A selected card that went offline blocks Start.
        let mut offline = cards.clone();
        let mut stale = Selection::default();
        stale.set_workspace(card(&cards, "local/coding"), 0, "~/web".into());
        offline[1].state = CardState::Offline;
        assert_eq!(
            stale.blocker(&offline).as_deref(),
            Some("local/coding is offline")
        );
    }

    #[test]
    fn two_workspaces_note_the_tool_prefixes() {
        let cards = cards(&sample());
        let mut selection = Selection::default();
        selection.set_workspace(card(&cards, "local/coding"), 0, "~/My Web App/".into());
        assert_eq!(selection.prefix_note(&cards), None);
        selection.toggle(card(&cards, "gpu/coding"));
        selection.set_workspace(card(&cards, "gpu/coding"), 0, "/srv/api".into());
        assert_eq!(
            selection.prefix_note(&cards).as_deref(),
            Some(
                "With 2 workspaces, their tools get prefixes: bash becomes local_my_web_app_bash."
            )
        );
        assert_eq!(attachment_name("local", "/"), "local");
        assert_eq!(attachment_name("local", "~"), "local");
        assert_eq!(
            attachment_name("labpc", r"C:\Users\Ada\Repo-1"),
            "labpc_repo_1"
        );
        assert_eq!(
            attachment_name("local", &format!("/x/{}", "a".repeat(40))).len(),
            32
        );
    }

    fn session(attachments: Vec<RuntimeAttachment>, minutes: i64) -> SessionMeta {
        let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": null,
            "cwd_identity": "/home/ada/web",
            "title": null,
            "last_agent_activity": null,
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": format!("sha256:{}", "1".repeat(64)),
            "catalog_revision": format!("sha256:{}", "2".repeat(64)),
            "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
            "model_revision": format!("sha256:{}", "4".repeat(64)),
            "agent_revision": format!("sha256:{}", "5".repeat(64)),
            "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
            "manifest_revision": format!("sha256:{}", "7".repeat(64)),
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-20T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta");
        meta.last_activity = meta
            .last_activity
            .checked_add(jiff::SignedDuration::from_mins(minutes))
            .unwrap();
        meta.attachments = attachments;
        meta
    }

    fn attachment(runtime: &str, capability: &str, workspace: Option<&str>) -> RuntimeAttachment {
        RuntimeAttachment {
            name: cookie_agent_protocol::AttachmentName::new(runtime).unwrap(),
            runtime_id: RuntimeId::new(runtime).unwrap(),
            capability: CapabilityName::new(capability).unwrap(),
            kind: RuntimeKind::new(capability).unwrap(),
            workspace: workspace.map(str::to_owned),
        }
    }

    fn homes() -> RuntimeHomes {
        let mut listing = sample();
        listing.runtimes[0].home = Some("/home/ada".into());
        RuntimeHomes::from_listing(&listing)
    }

    #[test]
    fn the_open_sessions_attachments_become_the_selection() {
        let attachments = vec![
            attachment("global", "global", None),
            attachment("local", "coding", Some("/home/ada/web")),
            attachment("local", "coding", Some("/srv/shared")),
            attachment("gpu", "coding", Some("/home/ada/api")),
        ];
        let selection = Selection::of_session(&attachments, &homes());
        assert!(selection.global);
        assert_eq!(
            selection.picks,
            [
                Pick {
                    runtime_id: "local".into(),
                    capability: Some("coding".into()),
                    workspaces: vec!["~/web".into(), "/srv/shared".into()],
                },
                Pick {
                    runtime_id: "gpu".into(),
                    capability: Some("coding".into()),
                    // gpu reported no home: absolute.
                    workspaces: vec!["/home/ada/api".into()],
                },
            ]
        );
        let without_global = Selection::of_session(&attachments[1..2], &homes());
        assert!(!without_global.global);
    }

    #[test]
    fn the_last_selection_round_trips_through_storage() {
        let mut selection = Selection::default();
        selection.picks[0].workspaces = vec!["~/web".into(), "~/docs".into()];
        selection.global = false;
        assert_eq!(
            Selection::from_storage(&selection.to_storage()),
            Some(selection)
        );
        assert_eq!(Selection::from_storage("not json"), None);
        assert_eq!(Selection::from_storage(r#"{"global": true}"#), None);
        // The first time ever: global and local, with an empty field.
        let first = Selection::default();
        assert!(first.global);
        assert_eq!(first.picks[0].runtime_id, "local");
        assert_eq!(first.picks[0].workspaces, [""]);
    }

    #[test]
    fn suggestions_come_from_this_browser_then_the_sessions_shown_under_home() {
        let sessions = vec![
            session(
                vec![attachment("local", "coding", Some("/home/ada/old"))],
                1,
            ),
            session(
                vec![
                    attachment("local", "coding", Some("/home/ada/web")),
                    attachment("gpu", "coding", Some("/srv/gpu")),
                ],
                5,
            ),
        ];
        let recent = vec!["/home/ada/web".to_owned(), "/opt/tools".to_owned()];
        assert_eq!(
            suggestions("local", &recent, &sessions, Some("/home/ada")),
            ["~/web", "/opt/tools", "~/old"]
        );
        assert_eq!(suggestions("gpu", &[], &sessions, None), ["/srv/gpu"]);
        assert!(suggestions("box", &[], &sessions, None).is_empty());
        let many = (0..20).map(|n| format!("/w/{n}")).collect::<Vec<_>>();
        assert_eq!(
            suggestions("local", &many, &[], None).len(),
            SUGGESTION_LIMIT
        );
    }

    #[test]
    fn remembering_a_workspace_moves_it_first_once() {
        let recent = vec!["/a".to_owned(), "/b".to_owned()];
        assert_eq!(remember(&recent, "/b"), ["/b", "/a"]);
        assert_eq!(remember(&recent, "/c"), ["/c", "/a", "/b"]);
        let many = (0..20).map(|n| format!("/w/{n}")).collect::<Vec<_>>();
        assert_eq!(remember(&many, "/new").len(), RECENT_LIMIT);
        assert_eq!(recent_from_storage(Some(r#"["/a"]"#)), ["/a"]);
        assert!(recent_from_storage(Some("{}")).is_empty());
        assert!(recent_from_storage(None).is_empty());
        assert_eq!(recent_key("local"), "cookie.recent_workspaces:local");
        assert_eq!(group_key("devbox"), "cookie.machine_open:devbox");
    }
}
