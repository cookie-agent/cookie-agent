//! An ACP agent document's `models` (`docs/site/specs/acp-agents.md`
//! section 15, document models): the first entry whose model the agent
//! offers picks its model option and the entry's variant its effort
//! option, over `acp.config` and under a session's own choices.

use cookie_agent_protocol::EventPayload;

use super::acp::*;
use super::acp_input::{choice, current, draft_options, options_set};
use crate::{Engine, SessionRuntimes};

fn session_diagnostics(engine: &Engine, session: cookie_agent_protocol::SessionId) -> Vec<String> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::Diagnostic { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn config_diagnostics(engine: &Engine) -> Vec<String> {
    engine.configuration_status().diagnostics
}

fn set(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_offered_model_and_its_variant_beat_acp_config_and_lose_to_session_choices() {
    let document = "---\ndescription: Fixture\nmode: all\nmodels: [{ model: huge, variant: high }, { model: large, variant: high }, small]\nacp:\n  config: { model: small, reasoning_effort: low }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    // Before the agent offered options, only its `model` option is known
    // to be the model.
    let diagnostics = config_diagnostics(engine);
    let precedence = |option: &str| {
        format!(
            "agent `fixture` sets `{option}` in `acp.config` and names `models`: `models` takes precedence"
        )
    };
    assert!(
        diagnostics
            .iter()
            .any(|d| d.starts_with(&precedence("model"))),
        "{diagnostics:?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.starts_with(&precedence("reasoning_effort"))),
        "{diagnostics:?}"
    );

    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    // `huge` is not offered, so `large` wins with its `high`; `acp.config`
    // sets neither.
    assert_eq!(
        options_set(&acp),
        set(&[
            ("mode", "auto"),
            ("model", "large"),
            ("reasoning_effort", "high")
        ])
    );
    assert!(
        !session_diagnostics(engine, session)
            .iter()
            .any(|d| d.contains("`models` was skipped")),
    );
    // The draft shows what a new session starts with.
    let options = draft_options(&acp);
    assert_eq!(current(&options, "model").as_deref(), Some("large"));
    assert_eq!(
        current(&options, "reasoning_effort").as_deref(),
        Some("high")
    );
    // Now the agent has said which option is its effort.
    assert!(
        config_diagnostics(engine)
            .iter()
            .any(|d| d.starts_with(&precedence("reasoning_effort")))
    );

    // A session's own choices still apply after the document's.
    let chosen = engine
        .create_session_with_agent_options(
            selection(),
            None,
            Vec::new(),
            SessionRuntimes::default(),
            None,
            vec![choice("reasoning_effort", "low")],
        )
        .await
        .expect("session created")
        .session_id;
    turn(engine, chosen, "second", "say hello").await;
    assert_eq!(
        options_set(&acp)[3..],
        set(&[
            ("mode", "auto"),
            ("model", "large"),
            ("reasoning_effort", "high"),
            ("reasoning_effort", "low"),
        ])[..]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_offered_model_is_a_notice_and_the_agent_keeps_its_own() {
    let document = "---\ndescription: Fixture\nmode: all\nmodels: [\"${small_model}\", { model: huge, variant: high }]\nacp:\n  config: { model: large }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let diagnostics = config_diagnostics(engine);
    assert!(
        diagnostics.iter().any(|d| d.starts_with(
            "agent `fixture` skips a `models` entry: `${small_model}` names one of cookie's models"
        )),
        "{diagnostics:?}"
    );
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    // `models` decide the model, so `acp.config`'s `large` is not set
    // either: the agent keeps its `small`.
    assert_eq!(options_set(&acp), set(&[("mode", "auto")]));
    let notices = session_diagnostics(engine, session);
    assert!(
        notices.iter().any(|d| d
            == "The agent document's `models` was skipped there: the agent's model option takes none of `huge`"),
        "{notices:?}"
    );
    assert_eq!(
        current(&draft_options(&acp), "model").as_deref(),
        Some("small")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_variant_the_agent_does_not_take_is_a_notice_after_the_model_is_set() {
    let document = "---\ndescription: Fixture\nmode: all\nmodels: [{ model: large, variant: extreme }]\nacp: {}\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    assert_eq!(
        options_set(&acp),
        set(&[("mode", "auto"), ("model", "large")])
    );
    let notices = session_diagnostics(engine, session);
    assert!(
        notices.iter().any(|d| d
            == "The agent document's `models` was skipped there: option `reasoning_effort` takes no value `extreme`"),
        "{notices:?}"
    );
    let options = draft_options(&acp);
    assert_eq!(current(&options, "model").as_deref(), Some("large"));
    assert_eq!(
        current(&options, "reasoning_effort").as_deref(),
        Some("low")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn models_naming_only_variables_leave_acp_config_in_charge() {
    let document = "---\ndescription: Fixture\nmode: all\nmodels: [\"${default_model}\"]\nacp:\n  config: { model: large }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let diagnostics = config_diagnostics(engine);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.contains("`${default_model}` names one of cookie's models")),
        "{diagnostics:?}"
    );
    assert!(
        !diagnostics.iter().any(|d| d.contains("takes precedence")),
        "{diagnostics:?}"
    );
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "say hello").await;
    assert_eq!(
        options_set(&acp),
        set(&[("mode", "auto"), ("model", "large")])
    );
}
