//! The Scheduled prompts view's rules beyond what every client shares
//! (`cookie_agent_client_state::cron`): which session "this session" is,
//! the template a new-session job starts from, the model pins the form
//! offers, what a run session's clock mark says, and the browser's local
//! time.

use cookie_agent_client_state::cron::{bare_template, job_title, model_pin_text};
use cookie_agent_protocol::{
    AgentId, AvailableModelDescriptor, CronJob, CronTemplate, ModelKey, ModelSelection, ProjectId,
    SessionCron, SessionId, SessionMeta, SessionOrigin, VariantId,
};
use jiff::{Timestamp, tz::TimeZone};

/// The session a job made from the ⋮ menu fires into: the open one, and
/// whether it is a subagent's. A session the list does not hold is a
/// subagent when it is not its tree's root.
pub fn session_target(
    sessions: &[SessionMeta],
    selected: Option<SessionId>,
    tree_root: Option<SessionId>,
) -> Option<(SessionId, bool)> {
    let selected = selected?;
    let subagent = sessions
        .iter()
        .find(|session| session.session_id == selected)
        .map_or_else(
            || tree_root.is_some_and(|root| root != selected),
            |session| matches!(session.origin, SessionOrigin::Delegated { .. }),
        );
    Some((selected, subagent))
}

/// A session's name for a job's target: its title, else its short ID.
pub fn session_name(sessions: &[SessionMeta], session_id: SessionId) -> Option<String> {
    sessions
        .iter()
        .find(|session| session.session_id == session_id)
        .and_then(|session| {
            session
                .title
                .as_ref()
                .map(ToString::to_string)
                .or_else(|| session.short_id.clone())
        })
}

/// The template a new-session job starts from: `agent` of `preset`, in
/// `project`; the daemon attaches what `session.create` would.
pub fn new_template(
    agent: AgentId,
    preset: Option<String>,
    project: Option<ProjectId>,
) -> CronTemplate {
    CronTemplate {
        preset,
        project,
        ..bare_template(agent)
    }
}

/// The model pins the form offers, as [`model_pin_text`] writes them: each
/// model's base, then its variants.
pub fn model_pins(models: &[AvailableModelDescriptor]) -> Vec<String> {
    pins_of(models.iter().map(|model| {
        (
            &model.key,
            model.variants.iter().map(|variant| &variant.id).collect(),
        )
    }))
}

fn pins_of<'a>(models: impl Iterator<Item = (&'a ModelKey, Vec<&'a VariantId>)>) -> Vec<String> {
    let mut pins = Vec::new();
    for (key, variants) in models {
        pins.push(model_pin_text(&ModelSelection {
            model: key.clone(),
            variant: None,
        }));
        for variant in variants {
            pins.push(model_pin_text(&ModelSelection {
                model: key.clone(),
                variant: Some(variant.clone()),
            }));
        }
    }
    pins
}

/// What a run session's clock mark says: the job that started it, by its
/// title while the daemon still has it.
pub fn clock_title(cron: &SessionCron, jobs: &[CronJob]) -> String {
    match jobs.iter().find(|job| job.id == cron.job_id) {
        Some(job) => format!("Started by the scheduled prompt {}", job_title(job)),
        None => format!(
            "Started by the scheduled prompt {}, since deleted",
            cron.job_id
        ),
    }
}

/// The browser's time zone at `at`: its offset then, so a fire after a
/// daylight-saving change shows its own offset. UTC outside a browser.
pub fn zone_at(at: Timestamp) -> TimeZone {
    TimeZone::fixed(offset_at(at))
}

#[cfg(target_arch = "wasm32")]
fn offset_at(at: Timestamp) -> jiff::tz::Offset {
    #[allow(clippy::cast_precision_loss, reason = "milliseconds within 2^53")]
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(at.as_millisecond() as f64));
    // Minutes behind UTC: -120 for UTC+2.
    #[allow(clippy::cast_possible_truncation, reason = "a whole number of minutes")]
    let behind = date.get_timezone_offset() as i32;
    jiff::tz::Offset::from_seconds(-behind * 60).unwrap_or(jiff::tz::Offset::UTC)
}

#[cfg(not(target_arch = "wasm32"))]
const fn offset_at(_: Timestamp) -> jiff::tz::Offset {
    jiff::tz::Offset::UTC
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{SessionCron, SessionId, SessionMeta};

    use super::{clock_title, model_pins, new_template, pins_of, session_name, session_target};

    fn session(id: u8, delegated: bool, title: Option<&str>) -> SessionMeta {
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
            "session_id": format!("0190a5b2-0000-7000-8000-00000000000{id}"),
            "origin": origin,
            "short_id": null,
            "cwd_identity": "/work",
            "title": title,
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": format!("sha256:{}", "1".repeat(64)),
            "catalog_revision": format!("sha256:{}", "2".repeat(64)),
            "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
            "model_revision": format!("sha256:{}", "4".repeat(64)),
            "agent_revision": format!("sha256:{}", "5".repeat(64)),
            "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
            "manifest_revision": format!("sha256:{}", "7".repeat(64)),
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-20T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta")
    }

    fn id(n: u8) -> SessionId {
        format!("0190a5b2-0000-7000-8000-00000000000{n}")
            .parse()
            .expect("session id")
    }

    #[test]
    fn the_open_session_is_the_target_and_a_delegated_one_a_subagent() {
        let sessions = [session(1, false, Some("Build")), session(2, true, None)];
        assert_eq!(session_target(&sessions, None, None), None);
        assert_eq!(
            session_target(&sessions, Some(id(1)), Some(id(1))),
            Some((id(1), false))
        );
        assert_eq!(
            session_target(&sessions, Some(id(2)), Some(id(1))),
            Some((id(2), true))
        );
        // Not listed: a subagent when it is not its tree's root.
        assert_eq!(
            session_target(&sessions, Some(id(3)), Some(id(1))),
            Some((id(3), true))
        );
        assert_eq!(
            session_target(&sessions, Some(id(3)), Some(id(3))),
            Some((id(3), false))
        );
        assert_eq!(session_name(&sessions, id(1)).as_deref(), Some("Build"));
        assert_eq!(session_name(&sessions, id(4)), None);
    }

    #[test]
    fn a_new_session_template_keeps_its_preset_and_project_and_attaches_global() {
        let template = new_template(
            "builder".parse().expect("agent"),
            Some("fast".into()),
            Some(
                "0190a5b2-0000-7000-8000-0000000000c1"
                    .parse()
                    .expect("project"),
            ),
        );
        assert_eq!(template.agent.as_str(), "builder");
        assert_eq!(template.preset.as_deref(), Some("fast"));
        assert!(template.project.is_some());
        assert!(template.global);
        assert_eq!(template.runtimes, None);
        assert_eq!(template.model, None);
    }

    #[test]
    fn pins_list_each_base_then_its_variants() {
        let key: cookie_agent_protocol::ModelKey =
            "deepseek/deepseek-flash".parse().expect("model key");
        let high = cookie_agent_protocol::VariantId::new("high").expect("variant");
        assert_eq!(
            pins_of([(&key, vec![&high])].into_iter()),
            ["deepseek/deepseek-flash", "deepseek/deepseek-flash[high]"]
        );
        assert!(model_pins(&[]).is_empty());
    }

    #[test]
    fn the_clock_names_its_job_or_says_it_is_gone() {
        let cron: SessionCron = serde_json::from_value(serde_json::json!({
            "job_id": "k3v9q2xa", "fired_at": "2026-10-23T03:00:00Z"
        }))
        .expect("cron");
        let job: cookie_agent_protocol::CronJob = serde_json::from_value(serde_json::json!({
            "id": "k3v9q2xa", "name": "Nightly", "prompt": "Check",
            "schedule": {"cron": "0 3 * * *"}, "max_runs": "unlimited",
            "target": {"new": {"agent": "builder"}}, "enabled": true,
            "created_by": "user", "created_at": "2026-10-22T18:00:00Z"
        }))
        .expect("job");
        assert_eq!(
            clock_title(&cron, std::slice::from_ref(&job)),
            "Started by the scheduled prompt Nightly"
        );
        assert_eq!(
            clock_title(&cron, &[]),
            "Started by the scheduled prompt k3v9q2xa, since deleted"
        );
    }
}
