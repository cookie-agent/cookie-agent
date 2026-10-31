//! The session list: `session.list` read on connecting and on a resync, and
//! kept current between reads by `sessions.changed`. A change that lands
//! while a read is on its way may postdate what that read saw, so it is
//! applied again to the read's answer when that lands; replaying one the
//! answer already holds is harmless, as every later change follows it.

use cookie_agent_client_state::refresh::{Latest, Ticket};
use cookie_agent_protocol::{SessionMeta, SessionOrigin, SessionsChangedNotification};

/// What a change did to the session list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Applied {
    /// The list differs: its readers must look again.
    pub changed: bool,
    /// A session moved between projects.
    pub moved: bool,
}

/// Applies an upsert or a removal to `sessions`: an upsert replaces a
/// session the list holds and adds a new root, and leaves a subagent it
/// does not hold out. An upsert of a session as the list holds it already
/// changes nothing.
pub fn apply_change(
    sessions: &mut Vec<SessionMeta>,
    change: &SessionsChangedNotification,
) -> Applied {
    match change {
        SessionsChangedNotification::Upserted { session } => {
            if let Some(existing) = sessions
                .iter_mut()
                .find(|existing| existing.session_id == session.session_id)
            {
                if *existing == **session {
                    return Applied::default();
                }
                let moved = existing.project != session.project;
                *existing = (**session).clone();
                Applied {
                    changed: true,
                    moved,
                }
            } else if matches!(session.origin, SessionOrigin::Root) {
                sessions.push((**session).clone());
                Applied {
                    changed: true,
                    moved: session.project.is_some(),
                }
            } else {
                Applied::default()
            }
        }
        SessionsChangedNotification::Removed { session_id } => {
            let before = sessions.len();
            sessions.retain(|session| session.session_id != *session_id);
            Applied {
                changed: sessions.len() != before,
                moved: false,
            }
        }
        _ => Applied::default(),
    }
}

/// The session list's reads, and the changes that came while the newest
/// was on its way.
#[derive(Debug, Default)]
pub struct ListReads {
    reads: Latest,
    pending: Option<Ticket>,
    since: Vec<SessionsChangedNotification>,
}

impl ListReads {
    /// Starts a read; an answer to an earlier one is dropped.
    pub fn begin(&mut self) -> Ticket {
        let read = self.reads.begin();
        self.pending = Some(read);
        self.since.clear();
        read
    }

    /// Notes `change`, applied to the list shown, for the answer on its
    /// way.
    pub fn note(&mut self, change: &SessionsChangedNotification) {
        if self.pending.is_some()
            && matches!(
                change,
                SessionsChangedNotification::Upserted { .. }
                    | SessionsChangedNotification::Removed { .. }
            )
        {
            self.since.push(change.clone());
        }
    }

    /// The list to show for `read`'s answer `sessions`, with the changes
    /// since it was asked for applied; `None` when a later read started.
    pub fn land(
        &mut self,
        read: Ticket,
        mut sessions: Vec<SessionMeta>,
    ) -> Option<Vec<SessionMeta>> {
        if !self.reads.is_current(read) {
            return None;
        }
        self.pending = None;
        for change in std::mem::take(&mut self.since) {
            apply_change(&mut sessions, &change);
        }
        Some(sessions)
    }

    /// `read` failed: nothing waits for the changes any more.
    pub fn fail(&mut self, read: Ticket) {
        if self.reads.is_current(read) {
            self.pending = None;
            self.since.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{SessionId, SessionMeta, SessionsChangedNotification};

    use super::{Applied, ListReads, apply_change};

    fn meta(title: &str, delegated: bool) -> SessionMeta {
        let origin = if delegated {
            serde_json::json!({
                "type": "delegated",
                "root_session_id": "0190a5b2-0000-7000-8000-000000000001",
                "parent_session_id": "0190a5b2-0000-7000-8000-000000000001",
                "parent_run_id": "0190a5b2-0000-7000-8000-0000000000a1",
                "parent_tool_call_id": "0190a5b2-0000-7000-8000-0000000000a2",
                "invocation_id": "0190a5b2-0000-7000-8000-0000000000b1",
                "depth": 1
            })
        } else {
            serde_json::json!({"type": "root"})
        };
        serde_json::from_value(serde_json::json!({
            "session_id": SessionId::new_v7(),
            "origin": origin,
            "short_id": null,
            "cwd_identity": "/repo",
            "title": title,
            "last_agent_activity": null,
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "catalog_revision": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "provider_state_revision": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            "model_revision": "sha256:4444444444444444444444444444444444444444444444444444444444444444",
            "agent_revision": "sha256:5555555555555555555555555555555555555555555555555555555555555555",
            "recipe_registry_revision": "sha256:6666666666666666666666666666666666666666666666666666666666666666",
            "manifest_revision": "sha256:7777777777777777777777777777777777777777777777777777777777777777",
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-20T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta")
    }

    fn upserted(session: &SessionMeta) -> SessionsChangedNotification {
        SessionsChangedNotification::Upserted {
            session: Box::new(session.clone()),
        }
    }

    fn titles(sessions: &[SessionMeta]) -> Vec<String> {
        sessions
            .iter()
            .map(|session| {
                session
                    .title
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            })
            .collect()
    }

    /// A session another client creates while `session.list` is on its
    /// way, and one it deletes: the answer, read before both, lands after
    /// them and still shows the new session and not the deleted one.
    #[test]
    fn changes_during_a_read_apply_to_its_answer() {
        let kept = meta("kept", false);
        let deleted = meta("deleted", false);
        let created = meta("created", false);
        let mut reads = ListReads::default();
        let mut shown = Vec::new();
        let read = reads.begin();
        for change in [
            upserted(&created),
            SessionsChangedNotification::Removed {
                session_id: deleted.session_id,
            },
        ] {
            reads.note(&change);
            apply_change(&mut shown, &change);
        }
        let shown = reads
            .land(read, vec![kept.clone(), deleted.clone()])
            .expect("the newest read");
        assert_eq!(titles(&shown), ["kept", "created"]);

        // Nothing is on its way now: a change is not kept for later.
        reads.note(&upserted(&meta("later", false)));
        let read = reads.begin();
        let shown = reads.land(read, vec![kept]).expect("the newest read");
        assert_eq!(titles(&shown), ["kept"]);
    }

    /// Two reads overlap: the earlier one's answer is dropped, and the
    /// later one's gets only the changes since it was asked for.
    #[test]
    fn an_older_read_is_dropped() {
        let first_session = meta("first", false);
        let mut reads = ListReads::default();
        let first = reads.begin();
        reads.note(&upserted(&first_session));
        let second = reads.begin();
        let renamed = meta("second", false);
        reads.note(&upserted(&renamed));
        assert_eq!(reads.land(first, Vec::new()), None);
        let shown = reads
            .land(second, vec![first_session])
            .expect("the newest read");
        assert_eq!(titles(&shown), ["first", "second"]);
    }

    #[test]
    fn a_failed_read_keeps_no_changes() {
        let mut reads = ListReads::default();
        let read = reads.begin();
        reads.note(&upserted(&meta("during", false)));
        reads.fail(read);
        let read = reads.begin();
        assert_eq!(reads.land(read, Vec::new()), Some(Vec::new()));
    }

    #[test]
    fn an_upsert_replaces_a_held_session_and_adds_only_roots() {
        let mut held = meta("before", false);
        let mut sessions = vec![held.clone()];
        held.title = Some(cookie_agent_protocol::SessionTitle::new("after").unwrap());
        assert_eq!(
            apply_change(&mut sessions, &upserted(&held)),
            Applied {
                changed: true,
                moved: false
            }
        );
        assert_eq!(
            apply_change(&mut sessions, &upserted(&meta("child", true))),
            Applied::default()
        );
        assert_eq!(titles(&sessions), ["after"]);
    }

    /// The daemon announces a session again as it was: the list's readers
    /// are not woken for it.
    #[test]
    fn an_upsert_or_removal_that_changes_nothing_says_so() {
        let held = meta("held", false);
        let mut sessions = vec![held.clone()];
        assert_eq!(
            apply_change(&mut sessions, &upserted(&held)),
            Applied::default()
        );
        let gone = SessionsChangedNotification::Removed {
            session_id: SessionId::new_v7(),
        };
        assert_eq!(apply_change(&mut sessions, &gone), Applied::default());
        let removed = SessionsChangedNotification::Removed {
            session_id: held.session_id,
        };
        assert!(apply_change(&mut sessions, &removed).changed);
        assert!(sessions.is_empty());
    }
}
