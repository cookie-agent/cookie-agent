//! Selecting several root sessions in the session list to mark them read,
//! mark them unread, or delete them (`docs/site/specs/unread-sessions.md`
//! section 7).

use std::collections::HashSet;

use cookie_agent_client_state::session_delete::{
    DAEMON_TOO_OLD_TO_DELETE, SessionDeleteOutcome, batch_delete_message, delete_session,
    skip_reason,
};
use cookie_agent_protocol::SessionId;
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

/// The selected roots, in the order they were selected, with a set beside
/// them: every row asks whether it is selected as the selection changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picked {
    order: Vec<SessionId>,
    set: HashSet<SessionId>,
}

impl Picked {
    /// How many are selected.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Whether `session_id` is selected.
    pub fn contains(&self, session_id: &SessionId) -> bool {
        self.set.contains(session_id)
    }

    /// The selection, in the order it was made.
    pub fn ids(&self) -> &[SessionId] {
        &self.order
    }

    /// Selects `session_id`, last, unless it is selected already.
    fn insert(&mut self, session_id: SessionId) {
        if self.set.insert(session_id) {
            self.order.push(session_id);
        }
    }

    /// Unselects `session_id`.
    fn remove(&mut self, session_id: &SessionId) {
        if self.set.remove(session_id) {
            self.order.retain(|id| id != session_id);
        }
    }

    /// Unselects every one of `members`.
    fn remove_all(&mut self, members: &[SessionId]) {
        let gone = members
            .iter()
            .filter(|id| self.set.remove(id))
            .copied()
            .collect::<HashSet<_>>();
        if !gone.is_empty() {
            self.order.retain(|id| !gone.contains(id));
        }
    }
}

/// The session list's selection of several sessions.
#[derive(Clone, Copy)]
pub struct Picking {
    /// Whether the list is in select mode.
    pub active: RwSignal<bool>,
    /// The selected roots.
    pub picked: RwSignal<Picked>,
    /// Whether the delete of the selection waits for confirmation.
    pub confirming: RwSignal<bool>,
    /// Whether the selection is being deleted.
    pub deleting: RwSignal<bool>,
    /// The row a Shift+click ranges from.
    anchor: StoredValue<Option<SessionId>>,
    /// The root rows the list shows, in page order, while it shows: what a
    /// Shift+click ranges over.
    pub shown: StoredValue<Option<Memo<Vec<SessionId>>>>,
}

impl Picking {
    pub(super) fn new() -> Self {
        Self {
            active: RwSignal::new(false),
            picked: RwSignal::new(Picked::default()),
            confirming: RwSignal::new(false),
            deleting: RwSignal::new(false),
            anchor: StoredValue::new(None),
            shown: StoredValue::new(None),
        }
    }
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
    fn of(picked: &Picked, members: &[SessionId]) -> Self {
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
        self.picking.picked.set(Picked::default());
        self.picking.confirming.set(false);
        self.picking.anchor.set_value(None);
        self.picking.active.set(true);
    }

    /// Leaves select mode and clears the selection.
    pub fn stop_selecting(self) {
        self.picking.active.set(false);
        self.picking.confirming.set(false);
        self.picking.picked.set(Picked::default());
        self.picking.anchor.set_value(None);
    }

    /// Whether `session_id` is selected.
    pub fn is_picked(self, session_id: SessionId) -> bool {
        self.picking
            .picked
            .with(|picked| picked.contains(&session_id))
    }

    fn is_picked_untracked(self, session_id: SessionId) -> bool {
        self.picking
            .picked
            .with_untracked(|picked| picked.contains(&session_id))
    }

    /// A click on `session_id`'s row in select mode: toggles it, or with
    /// Shift selects the rows from the last one toggled to it.
    pub fn toggle_pick(self, session_id: SessionId, range: bool) {
        self.picking.confirming.set(false);
        let anchor = self.picking.anchor.get_value().filter(|_| range);
        // Only a range reads the rows shown, from the list's own memo.
        let span = anchor.and_then(|anchor| {
            let rows = self.picking.shown.get_value()?.get_untracked();
            let from = rows.iter().position(|id| *id == anchor)?;
            let to = rows.iter().position(|id| *id == session_id)?;
            Some(rows[from.min(to)..=from.max(to)].to_vec())
        });
        self.picking.picked.update(|picked| match span {
            Some(span) => {
                for id in span {
                    picked.insert(id);
                }
            }
            None => {
                if picked.contains(&session_id) {
                    picked.remove(&session_id);
                } else {
                    picked.insert(session_id);
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
                picked.insert(id);
            }
        });
    }

    /// Clears the selection and stays in select mode.
    pub fn unpick_all(self) {
        self.picking.confirming.set(false);
        self.picking.picked.set(Picked::default());
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
                picked.remove_all(members);
            } else {
                for id in members {
                    picked.insert(*id);
                }
            }
        });
    }

    /// Drops a session that went away from the selection.
    pub(super) fn unpick(self, session_id: SessionId) {
        if self.is_picked_untracked(session_id) {
            self.picking
                .picked
                .update(|picked| picked.remove(&session_id));
        }
    }

    /// Marks the selection unread or read, and keeps it selected.
    pub fn mark_picked(self, unread: bool) {
        let picked = self
            .picking
            .picked
            .with_untracked(|picked| picked.ids().to_vec());
        if !picked.is_empty() {
            self.set_unread(picked, unread);
        }
    }

    /// The titles of up to five selected sessions, and how many more, for
    /// the delete's confirmation.
    pub fn picked_titles(self) -> String {
        let picked = self.picking.picked.with(|picked| picked.ids().to_vec());
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
        let picked = self
            .picking
            .picked
            .with_untracked(|picked| picked.ids().to_vec());
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
        let picked = |ids: &[SessionId]| {
            let mut picked = Picked::default();
            for id in ids {
                picked.insert(*id);
            }
            picked
        };
        assert_eq!(PickState::of(&picked(&[]), &[a, b]), PickState::None);
        assert_eq!(PickState::of(&picked(&[c]), &[a, b]), PickState::None);
        assert_eq!(PickState::of(&picked(&[c, a]), &[a, b]), PickState::Some);
        assert_eq!(PickState::of(&picked(&[b, c, a]), &[a, b]), PickState::All);
        assert_eq!(PickState::of(&picked(&[a]), &[]), PickState::None);
    }

    #[test]
    fn the_selection_keeps_its_order_and_each_session_once() {
        let [a, b, c] = [
            SessionId::new_v7(),
            SessionId::new_v7(),
            SessionId::new_v7(),
        ];
        let mut picked = Picked::default();
        for id in [b, a, b, c] {
            picked.insert(id);
        }
        assert_eq!(picked.ids(), [b, a, c]);
        picked.remove(&a);
        assert_eq!(picked.ids(), [b, c]);
        assert!(!picked.contains(&a) && picked.contains(&c));
        picked.remove_all(&[c, a]);
        assert_eq!(picked.ids(), [b]);
        assert_eq!(picked.len(), 1);
    }
}
