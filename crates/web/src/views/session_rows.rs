//! The session list's rows, keyed by session: a row is built when its
//! session first shows, moves when the order changes, and draws again only
//! when its own session changes. Each session's meta is in a signal of its
//! own, as the transcript's rows are: were every row to read the one list,
//! each change to any session would wake them all.

use std::collections::{HashMap, HashSet};

use cookie_agent_protocol::{SessionId, SessionMeta, SessionOrigin};
use leptos::prelude::*;

use leptos::ev;

use super::{LONG_PRESS, RowActions, session_label, session_row};
use crate::{browser, layout::Press, workspace::Workspace};

/// A finger's long press on a session row, shared by the rows (one finger
/// presses at a time): held still for [`LONG_PRESS`], it shows the row's
/// buttons at once, with a short buzz where the phone has one, and the
/// click its release brings opens nothing. A move, as a scroll makes,
/// cancels it. A mouse has hover and its right-click instead.
#[derive(Clone, Copy)]
pub(super) struct RowPress {
    press: StoredValue<Press>,
    timer: StoredValue<Option<TimeoutHandle>>,
}

impl RowPress {
    pub(super) fn new() -> Self {
        Self {
            press: StoredValue::new(Press::default()),
            timer: StoredValue::new(None),
        }
    }

    fn stop_timer(self) {
        if let Some(timer) = self.timer.try_update_value(Option::take).flatten() {
            timer.clear();
        }
    }

    /// A press on `session_id`'s row.
    pub(super) fn down(self, session_id: SessionId, event: &ev::PointerEvent, actions: RowActions) {
        if event.pointer_type() != "touch" {
            return;
        }
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        self.press.update_value(|press| press.start(x, y));
        self.stop_timer();
        browser::set_pressing(true);
        let timer = set_timeout_with_handle(
            move || {
                if self.press.try_update_value(Press::elapse).unwrap_or(false) {
                    Self::held(session_id, actions);
                }
            },
            LONG_PRESS,
        )
        .ok();
        self.timer.set_value(timer);
    }

    /// The press on `session_id`'s row was a long one: its buttons show.
    /// The browser's own long press, its `contextmenu`, says so too, and
    /// may come first, taking the gesture over.
    pub(super) fn held(session_id: SessionId, actions: RowActions) {
        if actions.revealed.get_untracked() == Some(session_id) {
            return;
        }
        actions.confirming.set(None);
        actions.revealed.set(Some(session_id));
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().vibrate_with_duration(12);
        }
    }

    /// The finger moved: far enough, it is no long press.
    pub(super) fn moved(self, event: &ev::PointerEvent) {
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        if self
            .press
            .try_update_value(|press| press.moved(x, y))
            .unwrap_or(false)
        {
            self.stop_timer();
            browser::set_pressing(false);
        }
    }

    /// The finger lifted, or the browser took the gesture over.
    pub(super) fn up(self) {
        self.press.update_value(Press::cancel);
        self.stop_timer();
        browser::set_pressing(false);
    }

    /// Whether the click that came opens the row: not the one a long
    /// press's release brings.
    pub(super) fn click(self) -> bool {
        self.press.try_update_value(Press::click).unwrap_or(true)
    }
}

/// Each listed session's meta, by ID.
#[derive(Clone, Copy)]
pub(super) struct SessionSlots {
    workspace: Workspace,
    map: StoredValue<HashMap<SessionId, ArcRwSignal<SessionMeta>>>,
}

impl SessionSlots {
    /// The slots of `workspace`'s session list, kept current as it changes.
    pub(super) fn new(workspace: Workspace) -> Self {
        let slots = Self {
            workspace,
            map: StoredValue::new(HashMap::new()),
        };
        Effect::new(move |_| workspace.sessions.with(|sessions| slots.sync(sessions)));
        slots
    }

    /// Sets each slot whose session changed, and forgets those of sessions
    /// gone from the list.
    fn sync(self, sessions: &[SessionMeta]) {
        let changed = self.map.with_value(|map| {
            sessions
                .iter()
                .filter_map(|session| {
                    let slot = map.get(&session.session_id)?;
                    slot.with_untracked(|shown| shown != session)
                        .then(|| (slot.clone(), session.clone()))
                })
                .collect::<Vec<_>>()
        });
        for (slot, session) in changed {
            slot.set(session);
        }
        if self.map.with_value(HashMap::is_empty) {
            return;
        }
        let live = sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<HashSet<_>>();
        if self
            .map
            .with_value(|map| map.keys().any(|id| !live.contains(id)))
        {
            self.map
                .update_value(|map| map.retain(|id, _| live.contains(id)));
        }
    }

    /// The slot of `session_id`, made from the list when it is new; `None`
    /// for a session the list does not hold.
    fn slot(self, session_id: SessionId) -> Option<ArcRwSignal<SessionMeta>> {
        if let Some(slot) = self.map.with_value(|map| map.get(&session_id).cloned()) {
            return Some(slot);
        }
        let session = self.workspace.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .cloned()
        })?;
        let slot = ArcRwSignal::new(session);
        self.map.update_value(|map| {
            map.insert(session_id, slot.clone());
        });
        Some(slot)
    }
}

/// The rows of `ids`, in their order, each keyed by its session.
pub(super) fn keyed_rows(
    rows: SessionRows,
    ids: impl Fn() -> Vec<SessionId> + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <For
            each=ids
            key=|id| *id
            children=move |id| rows.row(id)
        />
    }
}

/// What every row of the list shares.
#[derive(Clone, Copy)]
pub(super) struct SessionRows {
    pub workspace: Workspace,
    pub slots: SessionSlots,
    pub now: RwSignal<jiff::Timestamp>,
    pub actions: RowActions,
    pub press: RowPress,
}

impl SessionRows {
    /// `session_id`'s row, drawn again whenever its session changes.
    fn row(self, session_id: SessionId) -> impl IntoView {
        let Self {
            workspace,
            slots,
            now,
            actions,
            press,
        } = self;
        let slot = slots.slot(session_id);
        move || {
            slot.as_ref()
                .map(|slot| session_row(workspace, slot.get(), now, actions, press))
        }
    }
}

/// The lowercased titles the search looks in, by root session: worked out
/// as the list changes while a search is on, not at every key typed.
pub(super) fn search_labels(
    workspace: Workspace,
    searching: Memo<bool>,
) -> Memo<HashMap<SessionId, String>> {
    Memo::new(move |_| {
        if !searching.get() {
            return HashMap::new();
        }
        workspace.sessions.with(|sessions| {
            sessions
                .iter()
                .filter(|session| matches!(session.origin, SessionOrigin::Root))
                .map(|session| (session.session_id, session_label(session).to_lowercase()))
                .collect()
        })
    })
}

/// Whether the search `query` (lowercased) matches `session_id`'s title in
/// `labels`; everything matches no search.
pub(super) fn search_matches(
    labels: &HashMap<SessionId, String>,
    session_id: SessionId,
    query: Option<&str>,
) -> bool {
    query.is_none_or(|query| {
        labels
            .get(&session_id)
            .is_some_and(|label| label.contains(query))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_search_looks_in_the_lowercased_titles() {
        let [a, b] = [SessionId::new_v7(), SessionId::new_v7()];
        let labels = HashMap::from([(a, "fix the login".to_owned())]);
        assert!(search_matches(&labels, a, Some("login")));
        assert!(!search_matches(&labels, a, Some("logout")));
        // A session without a label matches only no search.
        assert!(!search_matches(&labels, b, Some("login")));
        assert!(search_matches(&labels, b, None));
    }
}
