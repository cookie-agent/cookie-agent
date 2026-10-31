//! Selecting several root sessions in the session list to mark them read,
//! mark them unread, or delete them (`docs/site/specs/unread-sessions.md`
//! section 7).

use cookie_agent_client_state::session_delete::{
    DAEMON_TOO_OLD_TO_DELETE, SessionDeleteOutcome, batch_delete_message, delete_session,
    skip_reason,
};
use cookie_agent_protocol::SessionId;
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::browser;

/// The root rows the session list shows, in page order.
const SHOWN_ROWS: &str = "nav[aria-label=Sessions] [data-session-row]";

/// The session list's selection of several sessions.
#[derive(Clone, Copy)]
pub struct Picking {
    /// Whether the list is in select mode.
    pub active: RwSignal<bool>,
    /// The selected roots, in the order they were selected.
    pub picked: RwSignal<Vec<SessionId>>,
    /// Whether the delete of the selection waits for confirmation.
    pub confirming: RwSignal<bool>,
    /// Whether the selection is being deleted.
    pub deleting: RwSignal<bool>,
    /// The row a Shift+click ranges from.
    anchor: StoredValue<Option<SessionId>>,
}

impl Picking {
    pub(super) fn new() -> Self {
        Self {
            active: RwSignal::new(false),
            picked: RwSignal::new(Vec::new()),
            confirming: RwSignal::new(false),
            deleting: RwSignal::new(false),
            anchor: StoredValue::new(None),
        }
    }
}

/// The rows the list shows, by session.
fn shown_rows() -> Vec<SessionId> {
    browser::attributes_in_order(SHOWN_ROWS, "data-session-row")
        .into_iter()
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// How many of a group of sessions are selected: none, some, or all (an
/// empty group counts as none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickState {
    None,
    Some,
    All,
}

impl PickState {
    fn of(picked: &[SessionId], members: &[SessionId]) -> Self {
        let count = members.iter().filter(|id| picked.contains(id)).count();
        if count == 0 {
            Self::None
        } else if count == members.len() {
            Self::All
        } else {
            Self::Some
        }
    }

    /// The checkbox's `aria-checked`.
    pub fn aria_checked(self) -> &'static str {
        match self {
            Self::None => "false",
            Self::Some => "mixed",
            Self::All => "true",
        }
    }
}

impl Workspace {
    /// Starts select mode with nothing selected.
    pub fn start_selecting(self) {
        self.picking.picked.set(Vec::new());
        self.picking.confirming.set(false);
        self.picking.anchor.set_value(None);
        self.picking.active.set(true);
    }

    /// Leaves select mode and clears the selection.
    pub fn stop_selecting(self) {
        self.picking.active.set(false);
        self.picking.confirming.set(false);
        self.picking.picked.set(Vec::new());
        self.picking.anchor.set_value(None);
    }

    /// Whether `session_id` is selected.
    pub fn is_picked(self, session_id: SessionId) -> bool {
        self.picking
            .picked
            .with(|picked| picked.contains(&session_id))
    }

    /// A click on `session_id`'s row in select mode: toggles it, or with
    /// Shift selects the rows from the last one toggled to it.
    pub fn toggle_pick(self, session_id: SessionId, range: bool) {
        self.picking.confirming.set(false);
        let anchor = self.picking.anchor.get_value();
        let rows = shown_rows();
        let span = anchor.filter(|_| range).and_then(|anchor| {
            let from = rows.iter().position(|id| *id == anchor)?;
            let to = rows.iter().position(|id| *id == session_id)?;
            Some((from, to))
        });
        self.picking.picked.update(|picked| match span {
            Some((from, to)) => {
                for id in &rows[from.min(to)..=from.max(to)] {
                    if !picked.contains(id) {
                        picked.push(*id);
                    }
                }
            }
            None => {
                if let Some(index) = picked.iter().position(|id| *id == session_id) {
                    picked.remove(index);
                } else {
                    picked.push(session_id);
                }
            }
        });
        self.picking.anchor.set_value(Some(session_id));
    }

    /// Selects `rows`, the root rows the list shows, in page order.
    pub fn pick_all(self, rows: Vec<SessionId>) {
        self.picking.confirming.set(false);
        self.picking.picked.update(|picked| {
            for id in rows {
                if !picked.contains(&id) {
                    picked.push(id);
                }
            }
        });
    }

    /// Clears the selection and stays in select mode.
    pub fn unpick_all(self) {
        self.picking.confirming.set(false);
        self.picking.picked.set(Vec::new());
        self.picking.anchor.set_value(None);
    }

    /// How many of `members` are selected.
    pub fn pick_state(self, members: &[SessionId]) -> PickState {
        self.picking
            .picked
            .with(|picked| PickState::of(picked, members))
    }

    /// A click on a project's checkbox: selects all of `members` (its
    /// sessions, folded ones included), or clears them when all are
    /// selected already.
    pub fn toggle_picks(self, members: &[SessionId]) {
        self.picking.confirming.set(false);
        self.picking.picked.update(|picked| {
            if PickState::of(picked, members) == PickState::All {
                picked.retain(|id| !members.contains(id));
            } else {
                for id in members {
                    if !picked.contains(id) {
                        picked.push(*id);
                    }
                }
            }
        });
    }

    /// Drops a session that went away from the selection.
    pub(super) fn unpick(self, session_id: SessionId) {
        self.picking.picked.update(|picked| {
            picked.retain(|id| *id != session_id);
        });
    }

    /// Marks the selection unread or read, and keeps it selected.
    pub fn mark_picked(self, unread: bool) {
        let picked = self.picking.picked.get_untracked();
        if !picked.is_empty() {
            self.set_unread(picked, unread);
        }
    }

    /// The titles of up to five selected sessions, and how many more, for
    /// the delete's confirmation.
    pub fn picked_titles(self) -> String {
        let picked = self.picking.picked.get();
        let titles = self.sessions.with(|sessions| {
            picked
                .iter()
                .take(5)
                .filter_map(|id| sessions.iter().find(|session| session.session_id == *id))
                .map(|session| format!("“{}”", crate::views::session_label(session)))
                .collect::<Vec<_>>()
        });
        let mut text = titles.join(", ");
        if picked.len() > 5 {
            text.push_str(&format!(" and {} more", picked.len() - 5));
        }
        text
    }

    /// Deletes the selected trees, each as its row showed it: a tree that
    /// cannot be deleted (running, changed since, open elsewhere) is
    /// skipped and stays selected, and the rest are still deleted.
    pub fn delete_picked(self) {
        let picked = self.picking.picked.get_untracked();
        let sessions = self.sessions.with_untracked(|sessions| {
            picked
                .iter()
                .filter_map(|id| sessions.iter().find(|session| session.session_id == *id))
                .cloned()
                .collect::<Vec<_>>()
        });
        let Some(client) = self.client() else {
            return;
        };
        self.picking.confirming.set(false);
        self.picking.deleting.set(true);
        spawn_local(async move {
            let mut deleted = 0;
            let mut skipped = Vec::new();
            for session in &sessions {
                let title = format!("“{}”", crate::views::session_label(session));
                match delete_session(&client, session).await {
                    SessionDeleteOutcome::Deleted(_) => {
                        deleted += 1;
                        self.unpick(session.session_id);
                    }
                    SessionDeleteOutcome::DaemonTooOld => {
                        self.status.set(DAEMON_TOO_OLD_TO_DELETE.to_owned());
                        self.picking.deleting.set(false);
                        return;
                    }
                    SessionDeleteOutcome::Failed(error) => {
                        skipped.push((title, skip_reason(&error)));
                    }
                }
            }
            self.picking.deleting.set(false);
            self.status.set(batch_delete_message(deleted, &skipped));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_picked_none_some_or_all() {
        let [a, b, c] = [
            SessionId::new_v7(),
            SessionId::new_v7(),
            SessionId::new_v7(),
        ];
        assert_eq!(PickState::of(&[], &[a, b]), PickState::None);
        assert_eq!(PickState::of(&[c], &[a, b]), PickState::None);
        assert_eq!(PickState::of(&[c, a], &[a, b]), PickState::Some);
        assert_eq!(PickState::of(&[b, c, a], &[a, b]), PickState::All);
        assert_eq!(PickState::of(&[a], &[]), PickState::None);
    }
}
