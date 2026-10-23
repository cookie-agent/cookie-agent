//! Onboarding: the notice while the daemon is not set up, "Not now", and
//! the `/onboard` panel's steps, model choices, and Finish.

use cookie_agent_client_state::onboarding::OnboardingStep;
use cookie_agent_protocol::OnboardingRequirement;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::app::onboarding::{ModelSlot, OnboardingAction, sample_status};
use crate::ui::app::*;
use crate::ui::slash::SlashCommand;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

async fn pump_until(app: &mut App, done: impl Fn(&App) -> bool) {
    for _ in 0..200 {
        if done(app) {
            return;
        }
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
                .await
                .expect("an update in time")
                .expect("rpc update");
        app.handle_rpc_update(update);
    }
    panic!("condition never held");
}

const GATEWAY_CONFIG: &str = r#"
[providers."custom.gateway"]
source = "custom"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.gateway".models."arbitrary-model"]
display_name = "Arbitrary Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 8192, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = false, top_p = false, seed = false, native_replay = "unsupported", media = {} }
"#;

#[tokio::test]
async fn a_daemon_that_is_not_set_up_is_set_up_from_the_panel() {
    let (directory, server) = crate::tests::in_process_server_with_configuration();
    // The daemon's config.toml on disk holds the provider it runs with, as a
    // real daemon's does; onboarding writes and applies that file.
    std::fs::write(
        directory.path().join(".cookie-agent/config.toml"),
        GATEWAY_CONFIG,
    )
    .expect("config.toml");
    let agents = directory.path().join(".cookie-agent/agents");
    std::fs::create_dir_all(&agents).expect("agents");
    std::fs::write(
        agents.join("primary.md"),
        "---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\"]\npermissions: {}\n---\nWork.\n",
    )
    .expect("primary.md");
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let _deliveries = app.take_deliveries();

    app.poll_onboarding();
    pump_until(&mut app, |app| app.onboarding.status.is_some()).await;
    let status = app.onboarding.status.clone().expect("status");
    assert_eq!(
        status.missing,
        [
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::SmallModel
        ]
    );
    assert!(app.onboarding_notice_visible());
    assert!(rendered_frame(&mut app, 120, 40).contains("Cookie is not set up"));

    // The panel opens on the first step not done, and Finish says why it
    // is disabled.
    app.run_command(SlashCommand::Onboard).await;
    assert_eq!(app.modal, Modal::Onboarding);
    assert_eq!(app.onboarding.step, Some(OnboardingStep::DefaultModel));
    assert_eq!(
        app.onboarding_finish_refusal().as_deref(),
        Some("Choose a default model, choose a small model first.")
    );
    let frame = rendered_frame(&mut app, 120, 40);
    assert!(frame.contains("✓ Provider"), "{frame}");
    assert!(frame.contains("· Default model"), "{frame}");

    // Enter hands over to the model picker, which returns to the panel on
    // the next step once a model is chosen.
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.modal, Modal::Models);
    assert_eq!(app.onboarding.picking, Some(ModelSlot::Default));
    app.choose_picker_entry(0).await;
    assert_eq!(app.modal, Modal::Onboarding);
    assert_eq!(app.onboarding.step, Some(OnboardingStep::SmallModel));
    assert!(app.onboarding.default_choice.is_some());
    // The kit step stays offline: its preview is already answered.
    app.onboarding.preview = Some(Err("offline".into()));
    app.handle_key(key(KeyCode::Enter)).await;
    assert_eq!(app.onboarding.picking, Some(ModelSlot::Small));
    app.choose_picker_entry(0).await;
    assert_eq!(app.onboarding.step, Some(OnboardingStep::Kit));
    assert_eq!(app.onboarding_finish_refusal(), None);

    app.handle_key(key(KeyCode::Char('f'))).await;
    pump_until(&mut app, |app| app.onboarding.summary.is_some()).await;
    let status = app.onboarding.status.clone().expect("status");
    assert!(status.ready, "{status:?}");
    let summary = app.onboarding.summary.clone().expect("summary");
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("Set default_model")),
        "{summary:?}"
    );
    assert!(!app.onboarding_notice_visible());
    // `primary` runs on the default model now: the summary offers a first
    // session on it.
    app.request_runtime_resync();
    pump_until(&mut app, |app| !app.runtime_resync_in_flight).await;
    assert!(
        app.onboarding_actions()
            .contains(&OnboardingAction::StartSession)
    );
    app.activate_onboarding_action(OnboardingAction::StartSession);
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.new_session_draft
            .as_ref()
            .map(|draft| draft.agent.as_str()),
        Some("primary")
    );
}

#[tokio::test]
async fn not_now_hides_the_notice_until_what_is_missing_changes() {
    let (_directory, server) = crate::tests::in_process_server_with_configuration();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let _deliveries = app.take_deliveries();
    let state = tempfile::tempdir().expect("state directory");
    let file = state.path().join("tui-state.json");
    app.onboarding = crate::ui::app::onboarding::OnboardingState::with_state_file(file.clone());

    app.install_onboarding_status(Some(sample_status(&[
        OnboardingRequirement::DefaultModel,
        OnboardingRequirement::SmallModel,
    ])));
    assert!(app.onboarding_notice_visible());
    rendered_frame(&mut app, 120, 40);
    let (rect, _) = app
        .hit_map
        .onboarding
        .notice
        .iter()
        .copied()
        .find(|(_, action)| *action == OnboardingAction::NotNow)
        .expect("Not now");
    app.handle_click(rect.x, rect.y).await;
    assert!(!app.onboarding_notice_visible());
    assert!(!rendered_frame(&mut app, 120, 40).contains("Cookie is not set up"));

    // Remembered on this machine.
    let remembered = crate::ui::app::onboarding::OnboardingState::with_state_file(file);
    assert_eq!(
        remembered.not_now,
        Some(vec![
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::SmallModel
        ])
    );

    // Once something else is missing, the notice is offered again.
    app.install_onboarding_status(Some(sample_status(&[OnboardingRequirement::SmallModel])));
    assert!(app.onboarding_notice_visible());
    // A ready daemon has no notice.
    app.install_onboarding_status(Some(sample_status(&[])));
    assert!(!app.onboarding_notice_visible());
}

#[tokio::test]
async fn the_model_steps_preselect_the_default_and_the_first_listed_variant() {
    let (_directory, server) = crate::tests::in_process_server_with_configuration();
    let client = server.connect_in_process();
    client.handshake().await.expect("handshake");
    let mut app = App::new(client).await.expect("app");
    let _deliveries = app.take_deliveries();
    let mut model = model_descriptor();
    let variants = ["fast", "medium", "slow"];
    model.variants = variants
        .iter()
        .map(|id| {
            serde_json::from_value(serde_json::json!({
                "id": id, "display_name": id, "origin": "authored"
            }))
            .expect("variant")
        })
        .collect();
    model.variant_order = variants
        .iter()
        .map(|id| cookie_agent_protocol::VariantId::new(*id).expect("variant id"))
        .collect();
    model.default_variant = Some(cookie_agent_protocol::VariantId::new("medium").unwrap());
    let key = model.key.clone();
    app.models = vec![model];
    let variant = |slot| {
        app.onboarding_preferred_variant(slot, &key)
            .map(|variant| variant.to_string())
    };
    assert_eq!(variant(ModelSlot::Default).as_deref(), Some("medium"));
    assert_eq!(variant(ModelSlot::Small).as_deref(), Some("fast"));
}
