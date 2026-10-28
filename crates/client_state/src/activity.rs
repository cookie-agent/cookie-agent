//! What a session list shows about each session's activity: running, waiting
//! on a producer, a problem with its last run, or nothing.

use cookie_agent_protocol::{HeldReason, Open, SemVer, SessionMeta, SessionStatus, WaitingOn};

/// The client protocol minor from which `SessionMeta` reports `waiting_on`.
pub const WAITING_ON_MINOR: u32 = 9;

/// Whether a daemon whose handshake reported `version` reports what sessions
/// wait on. Before the handshake nothing is known; from an older daemon a
/// session never appears to wait.
#[must_use]
pub fn daemon_reports_waiting(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= WAITING_ON_MINOR)
    })
}

/// A session's activity as a session row shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionActivity {
    /// A run is going. It takes precedence over everything else: a running
    /// session shows as running whatever it waits on.
    Running,
    /// No run is going and the session's automatic runs are held
    /// (`docs/site/specs/held-sessions.md`): it waits for the user to resume
    /// it. It takes precedence over waiting, which the hold stops.
    Held(Open<HeldReason>),
    /// No run is going, and these producers will start the next one without
    /// the user, in `SessionMeta::waiting_on` order. Waiting takes
    /// precedence over a problem with the last run, since the session will
    /// run again.
    Waiting(Vec<WaitingOn>),
    /// The last run failed, was cancelled, or was interrupted, and nothing
    /// will wake the session.
    Problem(SessionStatus),
    /// Idle or completed with nothing to wait on: a row shows no activity.
    None,
}

/// What a session row shows for `meta`: [`SessionActivity::Running`] over
/// [`SessionActivity::Waiting`] over [`SessionActivity::Problem`]. A status
/// this build does not know shows as no activity.
#[must_use]
pub fn session_activity(meta: &SessionMeta) -> SessionActivity {
    if meta.status == SessionStatus::Running {
        return SessionActivity::Running;
    }
    if let Some(reason) = &meta.held {
        return SessionActivity::Held(reason.clone());
    }
    if !meta.waiting_on.is_empty() {
        return SessionActivity::Waiting(meta.waiting_on.clone());
    }
    match meta.status {
        SessionStatus::Failed | SessionStatus::Cancelled | SessionStatus::Interrupted => {
            SessionActivity::Problem(meta.status)
        }
        SessionStatus::Idle
        | SessionStatus::Running
        | SessionStatus::Completed
        | SessionStatus::Unknown => SessionActivity::None,
    }
}

/// Whether a session is at work: running, or waiting on a producer that
/// will start its next run. A held session waits for the user and is not.
/// A folded project section still shows these.
#[must_use]
pub fn session_at_work(meta: &SessionMeta) -> bool {
    matches!(
        session_activity(meta),
        SessionActivity::Running | SessionActivity::Waiting(_)
    )
}

/// What a waiting row's tooltip says: `waiting on: goal, background call`,
/// each kind once, in `kinds` order.
#[must_use]
pub fn waiting_label(kinds: &[WaitingOn]) -> String {
    let mut names = Vec::<&str>::new();
    for kind in kinds {
        let name = match kind {
            WaitingOn::Goal => "goal",
            WaitingOn::BackgroundCall => "background call",
            WaitingOn::Subagent => "subagent",
            WaitingOn::ExternalProducer => "external producer",
            WaitingOn::ScheduledJob => "scheduled job",
            WaitingOn::Unknown => "another producer",
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    format!("waiting on: {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{HeldReason, Open, SemVer, SessionMeta, SessionStatus, WaitingOn};

    use super::{
        SessionActivity, daemon_reports_waiting, session_activity, session_at_work, waiting_label,
    };

    fn meta(status: SessionStatus, waiting_on: &[WaitingOn]) -> SessionMeta {
        let revision = format!("sha256:{}", "a".repeat(64));
        let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": "018f0000-0000-7000-8000-000000000001",
            "origin": {"type": "root"},
            "short_id": null,
            "cwd_identity": "/workspace",
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "provider/model", "variant": null},
                "preset": null
            },
            "runtime_revision": revision,
            "catalog_revision": revision,
            "provider_state_revision": revision,
            "model_revision": revision,
            "agent_revision": revision,
            "recipe_registry_revision": revision,
            "manifest_revision": revision,
            "title": null,
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-21T12:00:00Z",
            "status": status,
            "skipped_events": []
        }))
        .expect("session metadata");
        meta.waiting_on = waiting_on.to_vec();
        meta
    }

    #[test]
    fn held_beats_waiting_and_running_beats_held() {
        let stopped = Open::Known(HeldReason::Stopped {});
        let mut held = meta(SessionStatus::Cancelled, &[WaitingOn::Goal]);
        held.held = Some(stopped.clone());
        assert_eq!(session_activity(&held), SessionActivity::Held(stopped));
        held.status = SessionStatus::Running;
        assert_eq!(session_activity(&held), SessionActivity::Running);
    }

    #[test]
    fn running_and_waiting_sessions_are_at_work_and_a_held_one_is_not() {
        assert!(session_at_work(&meta(SessionStatus::Running, &[])));
        assert!(session_at_work(&meta(
            SessionStatus::Idle,
            &[WaitingOn::Goal]
        )));
        assert!(!session_at_work(&meta(SessionStatus::Idle, &[])));
        assert!(!session_at_work(&meta(SessionStatus::Failed, &[])));
        let mut held = meta(SessionStatus::Cancelled, &[WaitingOn::BackgroundCall]);
        held.held = Some(Open::Known(HeldReason::Stopped {}));
        assert!(!session_at_work(&held));
    }

    #[test]
    fn running_beats_waiting_beats_a_problem() {
        let goal = [WaitingOn::Goal];
        assert_eq!(
            session_activity(&meta(SessionStatus::Running, &goal)),
            SessionActivity::Running
        );
        assert_eq!(
            session_activity(&meta(SessionStatus::Running, &[])),
            SessionActivity::Running
        );
        for status in [
            SessionStatus::Idle,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
            SessionStatus::Interrupted,
        ] {
            assert_eq!(
                session_activity(&meta(
                    status,
                    &[WaitingOn::BackgroundCall, WaitingOn::Subagent]
                )),
                SessionActivity::Waiting(vec![WaitingOn::BackgroundCall, WaitingOn::Subagent]),
                "{status:?}"
            );
        }
        // A kind from a newer daemon still waits.
        assert_eq!(
            session_activity(&meta(SessionStatus::Completed, &[WaitingOn::Unknown])),
            SessionActivity::Waiting(vec![WaitingOn::Unknown])
        );
    }

    #[test]
    fn only_a_failed_cancelled_or_interrupted_run_is_a_problem() {
        for status in [
            SessionStatus::Failed,
            SessionStatus::Cancelled,
            SessionStatus::Interrupted,
        ] {
            assert_eq!(
                session_activity(&meta(status, &[])),
                SessionActivity::Problem(status)
            );
        }
        for status in [
            SessionStatus::Idle,
            SessionStatus::Completed,
            SessionStatus::Unknown,
        ] {
            assert_eq!(
                session_activity(&meta(status, &[])),
                SessionActivity::None,
                "{status:?}"
            );
        }
    }

    #[test]
    fn waiting_comes_from_a_daemon_of_minor_nine() {
        let reports = |version| daemon_reports_waiting(Some(version));
        assert!(!daemon_reports_waiting(None));
        assert!(!reports(SemVer::new(1, 8, 0)));
        assert!(reports(SemVer::new(1, 9, 0)));
        assert!(reports(SemVer::new(1, 12, 3)));
        assert!(!reports(SemVer::new(2, 9, 0)));
    }

    #[test]
    fn a_waiting_rows_tooltip_names_each_kind_once() {
        assert_eq!(
            waiting_label(&[WaitingOn::Goal, WaitingOn::BackgroundCall, WaitingOn::Goal]),
            "waiting on: goal, background call"
        );
        assert_eq!(
            waiting_label(&[WaitingOn::Subagent, WaitingOn::ExternalProducer]),
            "waiting on: subagent, external producer"
        );
    }
}
