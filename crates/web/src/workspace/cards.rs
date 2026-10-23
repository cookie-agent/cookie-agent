//! The new session's runtime cards, above its composer: what they offer
//! from `runtimes.list`, the machine groups the user opened, and where the
//! draft will work. The first message creates the session with the
//! selected cards (`Workspace::submit`). A draft starts with `global`
//! alone, or its project's defaults; the browser remembers no selection.

use std::collections::HashMap;

use cookie_agent_client_state::paths::RuntimeHomes;
use cookie_agent_protocol::{RuntimeAttachment, RuntimeId, SessionId};
use leptos::prelude::*;

use super::Workspace;
use crate::{
    browser,
    cards::{
        Card, Group, Selection, cards, daemon_machine, group_key, groups, recent_from_storage,
        recent_key, suggestions,
    },
};

/// A set of runtime cards and what the user picked on them: the new
/// session's, or the project editor's default attachments.
#[derive(Clone, Copy)]
pub struct CardSet {
    pub selection: RwSignal<Selection>,
    /// Refusals by card key and workspace field (the new session's only).
    pub errors: RwSignal<HashMap<(String, usize), String>>,
}

impl Workspace {
    /// The new session's cards.
    pub fn new_cards(self) -> CardSet {
        CardSet {
            selection: self.selection,
            errors: self.card_errors,
        }
    }

    /// Every runtime's home, from `runtimes.list` (none before it answered,
    /// or from a daemon older than client protocol 1.7).
    pub fn homes(self) -> RuntimeHomes {
        self.runtimes
            .with(|listing| listing.as_ref().map(RuntimeHomes::from_listing))
            .unwrap_or_default()
    }

    /// The attachments of `session_id`'s root session, as the session list
    /// has them.
    pub(super) fn root_attachments(self, session_id: SessionId) -> Option<Vec<RuntimeAttachment>> {
        let root = self.tree_root.get_untracked().unwrap_or(session_id);
        self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == root)
                .map(|session| session.attachments.clone())
                .filter(|attachments| !attachments.is_empty())
        })
    }

    /// The cards `runtimes.list` offers, in the groups' order.
    pub fn cards(self) -> Vec<Card> {
        self.card_groups()
            .into_iter()
            .flat_map(|group| group.cards)
            .collect()
    }

    /// The cards grouped by machine, this daemon's first.
    pub fn card_groups(self) -> Vec<Group> {
        self.runtimes
            .with(|listing| {
                listing
                    .as_ref()
                    .map(|listing| groups(&cards(listing), daemon_machine(listing).as_ref()))
            })
            .unwrap_or_default()
    }

    /// Whether `group` shows its cards in `set`.
    pub fn group_shown(self, group: &Group, set: CardSet) -> bool {
        let name = group.name();
        let stored = self
            .group_open
            .with(|open| open.get(&name).copied())
            .map(|open| if open { "open" } else { "closed" }.to_owned())
            .or_else(|| browser::load(&group_key(&name)));
        set.selection
            .with(|selection| group.open(stored.as_deref(), selection))
    }

    /// Opens or closes `group`, as the browser remembers.
    pub fn toggle_group(self, group: &Group, set: CardSet) {
        let open = !untrack(|| self.group_shown(group, set));
        let name = group.name();
        browser::store(&group_key(&name), if open { "open" } else { "closed" });
        self.group_open.update(|groups| {
            groups.insert(name, open);
        });
    }

    /// Selects or deselects `card` in `set`.
    pub fn toggle_card(self, card: &Card, set: CardSet) {
        set.selection.update(|selection| {
            selection.toggle(card);
        });
        let key = card.key();
        set.errors
            .update(|errors| errors.retain(|(failed, _), _| *failed != key));
        if set.selection == self.selection {
            self.draft_error.set(None);
        }
    }

    /// The workspaces a field of `runtime_id` suggests, as the fields show
    /// them: this browser's recent ones, then the sessions'.
    pub fn workspace_suggestions(self, runtime_id: &RuntimeId) -> Vec<String> {
        let recent =
            recent_from_storage(browser::load(&recent_key(runtime_id.as_str())).as_deref());
        let home = self.homes();
        self.sessions.with(|sessions| {
            suggestions(runtime_id.as_str(), &recent, sessions, home.get(runtime_id))
        })
    }

    /// What the new session will attach besides `global`, as typed on its
    /// cards (or its project's defaults while the draft keeps them): each
    /// runtime and workspace, in pick order, with fields left empty out.
    fn draft_places(self) -> Vec<(RuntimeId, Option<String>)> {
        self.selection.with(|selection| {
            selection
                .project_requests()
                .1
                .into_iter()
                .map(|request| {
                    let workspace = request.workspace.filter(|workspace| !workspace.is_empty());
                    (request.runtime_id, workspace)
                })
                .collect()
        })
    }

    /// The runtimes the bottom bar's workspaces are on, each once, in
    /// attach order: their colours mark the label.
    pub fn places_runtimes(self) -> Vec<String> {
        let mut runtimes = Vec::<String>::new();
        let mut add = |runtime_id: &RuntimeId| {
            if !runtimes.iter().any(|known| known == runtime_id.as_str()) {
                runtimes.push(runtime_id.to_string());
            }
        };
        if self.new_session.get() {
            for (runtime_id, workspace) in self.draft_places() {
                if workspace.is_some() {
                    add(&runtime_id);
                }
            }
            return runtimes;
        }
        let Some(session_id) = self.tree_root.get().or(self.selected.get()) else {
            return runtimes;
        };
        self.sessions.with(|sessions| {
            let attachments = sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.attachments.as_slice())
                .unwrap_or_default();
            for attachment in attachments {
                if attachment.workspace.is_some() {
                    add(&attachment.runtime_id);
                }
            }
        });
        runtimes
    }

    /// What the bottom bar says of where the viewed session (or the draft)
    /// works: its workspaces under their runtimes' homes; `None` before it
    /// has any runtimes.
    pub fn places_label(self) -> Option<String> {
        let homes = self.homes();
        if self.new_session.get() {
            let shown = self
                .draft_places()
                .into_iter()
                .filter_map(|(runtime_id, workspace)| {
                    let path = homes.display(&runtime_id, &workspace?);
                    Some(if runtime_id.as_str() == RuntimeId::LOCAL {
                        path
                    } else {
                        format!("{runtime_id}:{path}")
                    })
                })
                .collect::<Vec<_>>();
            return Some(if !shown.is_empty() {
                shown.join(", ")
            } else if self.selection.with(|selection| selection.global) {
                "global only: no workspace".to_owned()
            } else {
                "no workspace".to_owned()
            });
        }
        let session_id = self.tree_root.get().or(self.selected.get())?;
        self.sessions.with(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| {
                    crate::places::places_label(session, &homes, self.lists_attachments.get())
                })
        })
    }
}
