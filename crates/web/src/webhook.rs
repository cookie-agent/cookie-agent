//! The Webhooks view's rules beyond what every client shares
//! (`cookie_agent_client_state::webhook`): the template a new-session
//! webhook starts from, and what a webhook session's mark says.

use cookie_agent_client_state::webhook::webhook_title;
use cookie_agent_protocol::{AgentId, ProjectId, SessionWebhook, Webhook, WebhookTemplate};

/// The template a new-session webhook starts from: `agent` of `preset`, in
/// `project`; the daemon attaches what `session.create` would.
pub fn new_template(
    agent: AgentId,
    preset: Option<String>,
    project: Option<ProjectId>,
) -> WebhookTemplate {
    WebhookTemplate {
        preset,
        project,
        ..WebhookTemplate::for_agent(agent)
    }
}

/// What a webhook session's mark says: the webhook that started it, by
/// its title while the daemon still has it, and the thread it is.
pub fn mark_title(session: &SessionWebhook, webhooks: &[Webhook]) -> String {
    let thread = session
        .thread_key
        .as_deref()
        .map(|key| format!(", thread {key}"))
        .unwrap_or_default();
    match webhooks
        .iter()
        .find(|webhook| webhook.id == session.webhook_id)
    {
        Some(webhook) => format!("Started by the webhook {}{thread}", webhook_title(webhook)),
        None => format!(
            "Started by the webhook {}{thread}, since deleted",
            session.webhook_id
        ),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::SessionWebhook;

    use super::{mark_title, new_template};

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
        assert_eq!(template.thread_key, None);
        assert_eq!(template.model, None);
    }

    #[test]
    fn the_mark_finds_its_webhook_or_knows_it_is_gone() {
        let session: SessionWebhook = serde_json::from_value(serde_json::json!({
            "webhook_id": "w7k2m9qa", "event_id": "e3j8x0pq", "thread_key": "1234"
        }))
        .expect("session webhook");
        let gone = mark_title(&session, &[]);
        assert!(gone.contains("w7k2m9qa"));
        assert!(gone.contains("1234"));
        let unthreaded = SessionWebhook {
            thread_key: None,
            ..session
        };
        assert!(!mark_title(&unthreaded, &[]).contains("thread"));
    }
}
