//! History edits, as the TUI makes them: the menu of a user message (copy,
//! revert, fork), of a reply's end (revert, fork), and a compaction's
//! revert; and `/compact` with its cancel.

use cookie_agent_client_state::{TranscriptItem, cut_points::CutPoint};
use cookie_agent_protocol::{
    ClientForkId, SafeDisplayText, SessionCompactCancelParams, SessionCompactParams,
    SessionForkParams, SessionId, SessionRevertParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace, client_run_id};

/// The point of history a menu was opened on. A user message's text is
/// captured on opening: a rebuild (a concurrent revert) can change the
/// transcript before the action runs.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMenu {
    pub session_id: SessionId,
    pub point: CutPoint,
    /// The revert is waiting for its confirmation.
    pub confirming_revert: bool,
}

impl Workspace {
    /// Opens the menu of the viewed session's user message `seq`.
    pub fn open_user_menu(self, seq: u64) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        let text = self.store.with_value(|store| {
            store.sessions.get(&session_id).and_then(|state| {
                state.transcript.iter().find_map(|item| match item {
                    TranscriptItem::User {
                        seq: item_seq,
                        markdown,
                        ..
                    } if *item_seq == seq => Some(markdown.as_str().to_owned()),
                    _ => None,
                })
            })
        });
        let Some(text) = text else {
            self.status
                .set("message is no longer in the visible branch".into());
            return;
        };
        self.open_point_menu(session_id, CutPoint::UserMessage { seq, text }, false);
    }

    /// Opens the menu of the reply that ends with the viewed session's
    /// assistant block `item_id`.
    pub fn open_reply_menu(self, item_id: u64) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        let point = self.store.with_value(|store| {
            store
                .sessions
                .get(&session_id)
                .and_then(|state| state.reply_point(item_id))
        });
        let Some(point) = point else {
            self.status
                .set("reply is no longer in the visible branch".into());
            return;
        };
        self.open_point_menu(session_id, point, false);
    }

    /// Asks before reverting the viewed session to before its compaction
    /// committed at `seq`.
    pub fn open_compaction_revert(self, seq: u64) {
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        let point = self.store.with_value(|store| {
            store
                .sessions
                .get(&session_id)
                .and_then(|state| state.compaction_point(seq))
        });
        let Some(point) = point else {
            self.status
                .set("compaction is no longer in the visible branch".into());
            return;
        };
        self.open_point_menu(session_id, point, true);
    }

    fn open_point_menu(self, session_id: SessionId, point: CutPoint, confirming_revert: bool) {
        self.user_menu.set(Some(UserMenu {
            session_id,
            point,
            confirming_revert,
        }));
        self.panel.set(Some(Panel::Message));
    }

    /// Asks before a revert, which removes every later turn.
    pub fn confirm_revert(self, confirming: bool) {
        // A compaction's menu is its confirmation: backing out closes it.
        if !confirming
            && self.user_menu.with_untracked(|menu| {
                menu.as_ref()
                    .is_some_and(|menu| matches!(menu.point, CutPoint::Compaction { .. }))
            })
        {
            self.panel.set(None);
            self.user_menu.set(None);
            return;
        }
        self.user_menu.update(|menu| {
            if let Some(menu) = menu {
                menu.confirming_revert = confirming;
            }
        });
    }

    /// Reverts the session to the menu's point
    /// ([`CutPoint::through_seq`]), naming the tip this view shows so the
    /// daemon refuses instead of deleting turns another client added. The
    /// `rewound` notification rebuilds the transcript; a reverted
    /// message's text comes back into the composer.
    pub fn revert(self) {
        let (Some(client), Some(menu)) = (self.client(), self.user_menu.get_untracked()) else {
            return;
        };
        self.panel.set(None);
        self.user_menu.set(None);
        let Some(expected_last_seq) = self.store.with_value(|store| {
            store
                .sessions
                .get(&menu.session_id)
                .map(|state| state.last_seq)
        }) else {
            self.status.set("session is not loaded".into());
            return;
        };
        let through_seq = menu.point.through_seq();
        let text = menu.point.text().map(str::to_owned);
        spawn_local(async move {
            match client
                .revert_session(SessionRevertParams {
                    session_id: menu.session_id,
                    through_seq,
                    expected_last_seq,
                })
                .await
            {
                Ok(result) => {
                    let text = text.map(|text| result.instructions_override.unwrap_or(text));
                    if self.selected.get_untracked() == Some(menu.session_id) {
                        if let Some(text) = text {
                            self.put_in_composer(&text);
                            self.status
                                .set("reverted; the message text is back in the composer".into());
                        } else {
                            self.status.set("reverted".into());
                        }
                    }
                    self.refresh_tree();
                }
                Err(error) if error.is(&cookie_agent_protocol::errors::STALE_REVERT) => {
                    self.status.set(
                        "revert refused: the session changed since you opened this menu; \
                         review the new turns and revert again"
                            .into(),
                    );
                }
                Err(error) => self.status.set(format!(
                    "revert failed: {}",
                    crate::errors::describe(&error)
                )),
            }
        });
    }

    /// Adds `text` to the composer, after what it holds.
    fn put_in_composer(self, text: &str) {
        self.composer.update(|composer| {
            if composer.is_empty() {
                *composer = text.to_owned();
            } else {
                composer.push('\n');
                composer.push_str(text);
            }
        });
    }

    /// Forks the session at the menu's point ([`CutPoint::through_seq`]),
    /// then opens the fork. A fork at a user message leaves the message
    /// out and puts its text in the fork's composer, for editing there.
    pub fn fork(self) {
        let (Some(client), Some(menu)) = (self.client(), self.user_menu.get_untracked()) else {
            return;
        };
        self.panel.set(None);
        self.user_menu.set(None);
        self.status.set("forking the session…".into());
        let through_seq = menu.point.through_seq();
        let text = menu.point.text().map(str::to_owned);
        spawn_local(async move {
            match client
                .fork_session(SessionForkParams {
                    session_id: menu.session_id,
                    through_seq,
                    client_fork_id: ClientForkId::new(client_run_id().as_str())
                        .expect("a client run id is a valid fork id"),
                })
                .await
            {
                Ok(result) => {
                    let forked = result.session_id;
                    self.owned.update_value(|owned| {
                        owned.insert(forked);
                    });
                    self.read_only.update(|read_only| {
                        read_only.remove(&forked);
                    });
                    self.refresh_lists().await;
                    self.reroot_tree(forked);
                    self.open_session(forked);
                    if let Some(text) = text
                        && self.selected.get_untracked() == Some(forked)
                    {
                        self.put_in_composer(&text);
                        self.status
                            .set("forked the session; the message text is in its composer".into());
                    } else {
                        self.status.set("forked the session; switched to it".into());
                    }
                }
                Err(error) => self
                    .status
                    .set(format!("fork failed: {}", crate::errors::describe(&error))),
            }
        });
    }

    /// Copies the menu's message with `navigator.clipboard`; elsewhere the
    /// sheet's text is selected for the user to copy.
    pub fn copy_message(self) {
        let Some(text) = self
            .user_menu
            .get_untracked()
            .and_then(|menu| menu.point.text().map(str::to_owned))
        else {
            return;
        };
        spawn_local(async move {
            let copied = crate::browser::copy_text(&text).await;
            self.status.set(if copied {
                "message copied".into()
            } else {
                crate::browser::select_text("message-text");
                "copying needs a secure page; the text is selected, copy it with the keyboard"
                    .into()
            });
        });
    }

    /// Whether the viewed session compacts outside any run, which the
    /// gestures that cancel a run cancel.
    pub(super) fn compacting(self) -> bool {
        let Some(session_id) = self.selected.get_untracked() else {
            return false;
        };
        self.store.with_value(|store| {
            store
                .sessions
                .get(&session_id)
                .is_some_and(|state| state.active_run.is_none() && state.is_compacting())
        })
    }

    /// Whether the viewed session compacts its context now, tracked.
    pub fn compacting_now(self) -> bool {
        let Some(session_id) = self.selected.get() else {
            return false;
        };
        self.with_store(|store| {
            store
                .sessions
                .get(&session_id)
                .is_some_and(|state| state.active_run.is_none() && state.is_compacting())
        })
    }

    /// Compacts the viewed session's context, with an optional focus
    /// (`session.compact`).
    pub fn compact(self, focus: String) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            self.status
                .set("select a session before compacting context".into());
            return;
        };
        if !self.writable() {
            self.status.set("cannot compact a read-only session".into());
            return;
        }
        let focus = match (!focus.trim().is_empty())
            .then(|| SafeDisplayText::new(focus.trim()))
            .transpose()
        {
            Ok(focus) => focus,
            Err(_) => {
                self.status
                    .set("compaction focus must be control-free and at most 1024 bytes".into());
                return;
            }
        };
        self.status.set("compacting context…".into());
        spawn_local(async move {
            let status = match client
                .compact_session(SessionCompactParams { session_id, focus })
                .await
            {
                Ok(result) if result.compacted => "context compacted".into(),
                Ok(result) => result.cancellation_reason.unwrap_or_else(|| {
                    "context did not require or could not produce a smaller checkpoint".into()
                }),
                Err(error) => format!(
                    "context compaction failed: {}",
                    crate::errors::describe(&error)
                ),
            };
            if self.selected.get_untracked() == Some(session_id) {
                self.status.set(status);
            }
        });
    }

    /// Cancels the viewed session's compaction.
    pub fn cancel_compaction(self) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            let status = match client
                .cancel_compaction(SessionCompactCancelParams { session_id })
                .await
            {
                Ok(result) if result.cancelled => return,
                Ok(_) => "no compaction to cancel".into(),
                Err(error) => crate::errors::describe(&error),
            };
            self.status.set(status);
        });
    }
}
