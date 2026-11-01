//! What clients say about the daemon's status
//! (`docs/site/specs/daemon-startup.md` section 6): starting, a notice for
//! what it is busy with once ready, stopping, and a failed start. Every
//! task reads the same way, whatever it is.

use cookie_agent_protocol::{DaemonPhase, DaemonStatus, DaemonTask};

/// One task: "Moving sessions to the new storage layout: 41 of 97 trees
/// (2026-10)".
#[must_use]
pub fn task_text(task: &DaemonTask) -> String {
    let mut text = task.label.clone();
    if let Some(progress) = &task.progress {
        let unit = progress
            .unit
            .as_deref()
            .map(|unit| format!(" {unit}"))
            .unwrap_or_default();
        match progress.total {
            Some(total) => text.push_str(&format!(": {} of {total}{unit}", progress.done)),
            None => text.push_str(&format!(": {}{unit}", progress.done)),
        }
    }
    if let Some(detail) = task.detail.as_deref().filter(|detail| !detail.is_empty()) {
        text.push_str(&format!(" ({detail})"));
    }
    text
}

/// How far `task` is, from 0 to 1, when it knows its total.
#[must_use]
pub fn task_fraction(task: &DaemonTask) -> Option<f64> {
    let progress = task.progress.as_ref()?;
    let total = progress.total.filter(|total| *total > 0)?;
    #[allow(clippy::cast_precision_loss)]
    Some((progress.done.min(total) as f64) / (total as f64))
}

/// Whether the daemon has not opened yet: only its status works.
#[must_use]
pub fn is_starting(status: &DaemonStatus) -> bool {
    status.phase == DaemonPhase::Starting
}

/// The line for a daemon that is starting: "Cookie is starting: Moving
/// sessions…: 41 of 97 trees", naming its newest blocking task.
#[must_use]
pub fn starting_text(status: &DaemonStatus) -> String {
    status.blocking().last().map_or_else(
        || "Cookie is starting…".to_owned(),
        |task| format!("Cookie is starting: {}", task_text(task)),
    )
}

/// Why the daemon could not start: the error's message, then its detail.
#[must_use]
pub fn failed_text(status: &DaemonStatus) -> Option<String> {
    if status.phase != DaemonPhase::Failed {
        return None;
    }
    let Some(error) = &status.error else {
        return Some("Cookie could not start.".to_owned());
    };
    let detail = error
        .data
        .as_ref()
        .and_then(|data| data.get("detail"))
        .and_then(serde_json::Value::as_str)
        .filter(|detail| !detail.is_empty());
    Some(match detail {
        Some(detail) => format!("{} {detail}", error.message),
        None => error.message.clone(),
    })
}

/// The one-line notice for a daemon that is not simply ready and idle:
/// starting, failed, stopping, or busy with tasks that do not block it.
/// `None` when there is nothing to say.
#[must_use]
pub fn notice_text(status: &DaemonStatus) -> Option<String> {
    match status.phase {
        DaemonPhase::Starting => Some(starting_text(status)),
        DaemonPhase::Failed => failed_text(status),
        DaemonPhase::Stopping if status.restart => Some("Cookie is restarting…".to_owned()),
        DaemonPhase::Stopping => Some("Cookie is stopping…".to_owned()),
        DaemonPhase::Ready | DaemonPhase::Unknown => (!status.tasks.is_empty()).then(|| {
            status
                .tasks
                .iter()
                .map(task_text)
                .collect::<Vec<_>>()
                .join("; ")
        }),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{DaemonPhase, DaemonStatus, DaemonTask, TaskProgress, errors};

    use super::*;

    fn task(id: &str, label: &str, blocking: bool) -> DaemonTask {
        DaemonTask {
            id: id.into(),
            label: label.into(),
            detail: None,
            progress: None,
            blocking,
            started_at: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_task_reads_with_its_progress_and_detail() {
        let mut moving = task("1", "Moving sessions", true);
        assert_eq!(task_text(&moving), "Moving sessions");
        moving.progress = Some(TaskProgress {
            done: 41,
            total: Some(97),
            unit: Some("trees".into()),
        });
        moving.detail = Some("2026-10".into());
        assert_eq!(
            task_text(&moving),
            "Moving sessions: 41 of 97 trees (2026-10)"
        );
        assert!((task_fraction(&moving).unwrap() - 41.0 / 97.0).abs() < 1e-9);
        moving.progress = Some(TaskProgress {
            done: 3,
            total: None,
            unit: None,
        });
        assert_eq!(task_text(&moving), "Moving sessions: 3 (2026-10)");
        assert_eq!(task_fraction(&moving), None);
    }

    #[test]
    fn each_phase_has_its_notice() {
        let mut status = DaemonStatus {
            phase: DaemonPhase::Starting,
            tasks: vec![
                task("1", "Loading the catalog", true),
                task("2", "Opening the session store", true),
            ],
            ..DaemonStatus::default()
        };
        assert_eq!(
            notice_text(&status).as_deref(),
            Some("Cookie is starting: Opening the session store")
        );
        status.phase = DaemonPhase::Ready;
        status.tasks = vec![task("3", "Restarting when idle", false)];
        assert_eq!(
            notice_text(&status).as_deref(),
            Some("Restarting when idle")
        );
        status.tasks.clear();
        assert_eq!(notice_text(&status), None);
        status.phase = DaemonPhase::Stopping;
        status.restart = true;
        assert_eq!(
            notice_text(&status).as_deref(),
            Some("Cookie is restarting…")
        );
        let failed = DaemonStatus {
            phase: DaemonPhase::Failed,
            error: Some(
                errors::DAEMON_START_FAILED
                    .error()
                    .detail("the store is newer")
                    .build(),
            ),
            ..DaemonStatus::default()
        };
        assert_eq!(
            notice_text(&failed).as_deref(),
            Some("Cookie could not start. the store is newer")
        );
    }
}
