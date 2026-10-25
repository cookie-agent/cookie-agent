//! Restarting the daemon from a client
//! (`docs/site/specs/engine-configuration.md` section 11): the banner while
//! the configuration waits for a restart, the confirmation's two choices,
//! the waiting state, and what `daemon.restart` answered. Every client words
//! them the same way.

use cookie_agent_protocol::{
    ClientError, ConfigRestartPart, ConfigStatus, DaemonRestartResult,
    DaemonRestartingNotification, RestartWhen, SemVer, SessionMeta, SessionStatus,
};

/// The first client protocol whose daemons answer `daemon.restart`.
pub const DAEMON_RESTART_SINCE: SemVer = SemVer::new(1, 19, 0);

/// The confirmation's default choice.
pub const RESTART_WHEN_IDLE: &str = "Restart when idle";

/// What a daemon too old for `daemon.restart` is told.
pub const DAEMON_TOO_OLD_TO_RESTART: &str =
    "This daemon is too old to restart from a client; restart it where it runs.";

/// Whether the daemon, speaking `version`, answers `daemon.restart`.
#[must_use]
pub fn daemon_can_restart(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| version >= DAEMON_RESTART_SINCE)
}

/// The parts waiting for a restart, as a list: `plugins, runtimes`.
#[must_use]
pub fn restart_parts(status: &ConfigStatus) -> String {
    status
        .restart_required
        .iter()
        .map(|part| match part {
            ConfigRestartPart::Server => "server",
            ConfigRestartPart::Plugins => "plugins",
            ConfigRestartPart::Runtimes => "runtimes",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The banner text while configuration waits for a restart:
/// `Restart needed to apply: plugins, runtimes`; `None` when nothing does.
#[must_use]
pub fn restart_banner(status: &ConfigStatus) -> Option<String> {
    (!status.restart_required.is_empty())
        .then(|| format!("Restart needed to apply: {}", restart_parts(status)))
}

/// How many turns a restart now would interrupt, as far as the client's
/// session list knows: its running sessions.
#[must_use]
pub fn running_turns<'a>(sessions: impl IntoIterator<Item = &'a SessionMeta>) -> usize {
    sessions
        .into_iter()
        .filter(|session| session.status == SessionStatus::Running)
        .count()
}

fn turns(count: usize) -> String {
    if count == 1 {
        "1 running turn".to_owned()
    } else {
        format!("{count} running turns")
    }
}

/// The confirmation's other choice: `Restart now (interrupts 2 running
/// turns)`, or `Restart now` when none runs.
#[must_use]
pub fn restart_now_label(running: usize) -> String {
    if running == 0 {
        "Restart now".to_owned()
    } else {
        format!("Restart now (interrupts {})", turns(running))
    }
}

/// What the confirmation explains above its choices.
#[must_use]
pub fn restart_explanation(running: usize) -> String {
    let wait = if running == 0 {
        "No turn is running, so a restart when idle happens at once.".to_owned()
    } else {
        format!(
            "When idle waits for {} to finish and refuses new turns meanwhile; now \
             interrupts {}.",
            turns(running),
            if running == 1 { "it" } else { "them" }
        )
    };
    format!(
        "The daemon relaunches itself with the same arguments, applying the configuration \
         that needs a restart and any new cookie binary. {wait} Clients reconnect on their own."
    )
}

/// The waiting state: `Restarting when idle… (1 running turn)`, then
/// `Restarting…` once it is due.
#[must_use]
pub fn restarting_text(restarting: &DaemonRestartingNotification) -> String {
    if restarting.when == RestartWhen::Idle && restarting.active_runs > 0 {
        format!(
            "Restarting when idle… ({})",
            turns(usize::try_from(restarting.active_runs).unwrap_or(usize::MAX))
        )
    } else {
        "Restarting…".to_owned()
    }
}

/// What `daemon.restart` answered.
#[derive(Debug)]
pub enum RestartOutcome {
    Scheduled {
        when: RestartWhen,
        result: DaemonRestartResult,
    },
    /// `restart_unsupported`: the daemon cannot relaunch itself.
    Unsupported,
    Failed(String),
}

impl RestartOutcome {
    #[must_use]
    pub fn from_result(
        when: RestartWhen,
        result: Result<DaemonRestartResult, ClientError>,
    ) -> Self {
        match result {
            Ok(result) => Self::Scheduled { when, result },
            Err(error) if error.is(&cookie_agent_protocol::errors::RESTART_UNSUPPORTED) => {
                Self::Unsupported
            }
            Err(error) => Self::Failed(error.to_string()),
        }
    }

    /// The status line for it.
    #[must_use]
    pub fn status(&self) -> String {
        match self {
            Self::Scheduled { when, result } => {
                let active = usize::try_from(result.active_runs).unwrap_or(usize::MAX);
                match (when, active) {
                    (_, 0) => "Restarting…".to_owned(),
                    (RestartWhen::Now, active) => {
                        format!("Restarting now, interrupting {}…", turns(active))
                    }
                    (_, active) => format!("Restarting when idle… ({})", turns(active)),
                }
            }
            Self::Unsupported => "This daemon can't restart itself; it was not started by \
                                  `cookie daemon`."
                .to_owned(),
            Self::Failed(error) => format!("Restart failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::ConfigRevision;

    use super::*;

    fn status(parts: Vec<ConfigRestartPart>) -> ConfigStatus {
        ConfigStatus {
            applied_revision: ConfigRevision::new(format!("sha256:{}", "1".repeat(64)))
                .expect("revision"),
            restart_required: parts,
            error: None,
            diagnostics: Vec::new(),
            native_compaction: Vec::new(),
        }
    }

    #[test]
    fn the_banner_names_the_parts_waiting_for_a_restart() {
        assert_eq!(restart_banner(&status(Vec::new())), None);
        assert_eq!(
            restart_banner(&status(vec![
                ConfigRestartPart::Plugins,
                ConfigRestartPart::Runtimes
            ]))
            .as_deref(),
            Some("Restart needed to apply: plugins, runtimes")
        );
    }

    #[test]
    fn the_choices_and_the_wait_count_running_turns() {
        assert_eq!(restart_now_label(0), "Restart now");
        assert_eq!(
            restart_now_label(1),
            "Restart now (interrupts 1 running turn)"
        );
        assert_eq!(
            restart_now_label(3),
            "Restart now (interrupts 3 running turns)"
        );
        let waiting = |when, active_runs| DaemonRestartingNotification {
            when,
            active_runs,
            reason: None,
        };
        assert_eq!(
            restarting_text(&waiting(RestartWhen::Idle, 2)),
            "Restarting when idle… (2 running turns)"
        );
        assert_eq!(
            restarting_text(&waiting(RestartWhen::Idle, 0)),
            "Restarting…"
        );
        assert_eq!(
            restarting_text(&waiting(RestartWhen::Now, 2)),
            "Restarting…"
        );
    }

    #[test]
    fn only_daemons_of_1_19_restart_from_a_client() {
        assert!(!daemon_can_restart(None));
        assert!(!daemon_can_restart(Some(SemVer::new(1, 18, 0))));
        assert!(daemon_can_restart(Some(SemVer::new(1, 19, 0))));
        assert!(daemon_can_restart(Some(SemVer::new(1, 20, 3))));
    }

    #[test]
    fn an_unsupported_restart_says_why() {
        let scheduled = RestartOutcome::Scheduled {
            when: RestartWhen::Idle,
            result: DaemonRestartResult {
                scheduled: true,
                active_runs: 1,
            },
        };
        assert_eq!(scheduled.status(), "Restarting when idle… (1 running turn)");
        assert!(
            RestartOutcome::Unsupported
                .status()
                .contains("can't restart")
        );
    }
}
