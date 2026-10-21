//! History edits, as the TUI makes them: a user message's menu (copy,
//! revert, fork), and `/compact` with its cancel.

use cookie_agent_client_state::TranscriptItem;
use cookie_agent_protocol::{
    ClientForkId, SafeDisplayText, SessionCompactCancelParams, SessionCompactParams,
    SessionForkParams, SessionId, SessionRevertParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::{Panel, Workspace, client_run_id};

/// The message a menu was opened on. Its text is captured on opening: a
/// rebuild (a concurrent revert) can change the transcript before the
/// action runs.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMenu {
    pub session_id: SessionId,
    /// The physical sequence of the message's `user_input_submitted`.
    pub seq: u64,
    pub text: String,
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
        self.user_menu.set(Some(UserMenu {
            session_id,
            seq,
            text,
            confirming_revert: false,
        }));
        self.panel.set(Some(Panel::Message));
    }

    /// Asks before a revert, which removes the message and every later
    /// turn.
    pub fn confirm_revert(self, confirming: bool) {
        self.user_menu.update(|menu| {
            if let Some(menu) = menu {
                menu.confirming_revert = confirming;
            }
        });
    }

    /// Reverts the session to just before the menu's message
    /// (`through_seq = seq - 1`), naming the tip this view shows so the
    /// daemon refuses instead of deleting turns another client added. The
    /// `rewound` notification rebuilds the transcript; the message text
    /// comes back into the composer.
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
        // User messages always follow `session_created` (sequence 1).
        let through_seq = menu.seq.saturating_sub(1).max(1);
        self.status.set("reverting to before the message…".into());
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
                    let text = result.instructions_override.unwrap_or(menu.text);
                    if self.selected.get_untracked() == Some(menu.session_id) {
                        self.composer.update(|composer| {
                            if composer.is_empty() {
                                *composer = text;
                            } else {
                                composer.push('\n');
                                composer.push_str(&text);
                            }
                        });
                        self.status
                            .set("reverted; the message text is back in the composer".into());
                    }
                    self.refresh_tree();
                }
                Err(error) if error.is(&cookie_agent_protocol::errors::STALE_REVERT) => {
                    self.status.set(
                        "revert refused: the session changed since you opened this message; \
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

    /// Forks the session at the menu's message (`through_seq = seq`,
    /// keeping the message), then opens the fork.
    pub fn fork(self) {
        let (Some(client), Some(menu)) = (self.client(), self.user_menu.get_untracked()) else {
            return;
        };
        self.panel.set(None);
        self.user_menu.set(None);
        self.status
            .set("forking the session from the message…".into());
        spawn_local(async move {
            match client
                .fork_session(SessionForkParams {
                    session_id: menu.session_id,
                    through_seq: menu.seq,
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
                    self.status.set("forked the session; switched to it".into());
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
        let Some(menu) = self.user_menu.get_untracked() else {
            return;
        };
        spawn_local(async move {
            let copied = crate::browser::copy_text(&menu.text).await;
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

    /// Compacts the viewed session's context (`/compact [focus]`).
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
    pub(super) fn cancel_compaction(self) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            let status = match client
                .cancel_compaction(SessionCompactCancelParams { session_id })
                .await
            {
                Ok(result) if result.cancelled => "compaction cancellation requested".into(),
                Ok(_) => "no compaction to cancel".into(),
                Err(error) => crate::errors::describe(&error),
            };
            self.status.set(status);
        });
    }
}
