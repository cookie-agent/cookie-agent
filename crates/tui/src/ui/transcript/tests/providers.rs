use std::{sync::Arc, time::Duration};

use cookie_agent_protocol::{
    AgentId, ModelSelection, ProviderId, RunSelection, SafeCode, SafeDisplayText, SafeErrorMessage,
    SessionId,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use jiff::Timestamp;

use crate::Client;

use crate::state::SessionState;

use crate::ui::app::*;

use crate::ui::input::credential_wipe_count;

use crate::ui::pickers::SearchPickerFocus;

use crate::ui::provider::{ProviderAction, ProviderForm, ProviderFormFocus, ProviderOperation};

use crate::ui::slash::SlashCommand;

use super::support::*;

#[tokio::test]
async fn connect_submission_renders_the_provider_panel_with_an_empty_catalog() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.providers.clear();
    app.selected = Some(SessionId::new_v7());
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });

    type_input(&mut app, "/connect").await;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    assert_eq!(app.modal, Modal::ConnectProviders);
    assert!(app.input.as_str().is_empty());
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Search · Down/Tab/Enter: results"));
    assert!(rendered.contains("Connect provider (0/0) · Enter: details"));
    assert!(rendered.contains("No providers are available in the runtime snapshot."));
    assert!(rendered_cursor_visible(&mut app, 100, 30));
    assert!(app.hit_map.picker.is_some());
    assert!(app.hit_map.picker_input.is_some());
    assert!(app.hit_map.picker_rows.is_empty());
    tokio::task::yield_now().await;
    assert_eq!(recorded_method_count(&recorded, "run.start"), 0);
    assert_eq!(recorded_method_count(&recorded, "run.steer"), 0);
    drop(incoming_guard);

    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.query(), "x");
    assert_eq!(app.picker_state.selected(), None);
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.picker_state.selected(), None);
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Connect provider (0/0) · Enter: details"));
    assert!(rendered.contains('x'));
}

#[tokio::test]
async fn provider_search_accepts_non_ascii_typing_and_paste() {
    let mut app = test_app().await;
    let mut provider = catalog_provider();
    provider.display_name = SafeDisplayText::new("阿里云").expect("provider display name");
    app.providers = vec![provider];

    type_input(&mut app, "/connect").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    assert_eq!(app.modal, Modal::ConnectProviders);
    type_input(&mut app, "阿里").await;
    app.handle_paste("云");
    assert_eq!(app.provider_search.query(), "阿里云");
    assert_eq!(app.filtered_providers().len(), 1);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Connect provider (1/1) · Enter: details"));
    assert!(rendered.contains("阿 里 云"));
    assert!(app.hit_map.picker.is_some());
    assert_eq!(app.hit_map.picker_rows.len(), 1);

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    type_input(&mut app, "missing").await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Connect provider (0/1) · Enter: details"));
    assert!(rendered.contains("No providers match the filter."));
    assert!(app.hit_map.picker.is_some());
    assert!(app.hit_map.picker_rows.is_empty());
}

#[tokio::test]
async fn provider_search_edits_at_the_cursor_and_transitions_focus() {
    let mut app = test_app().await;
    app.providers = vec![catalog_provider()];
    app.run_command(SlashCommand::Connect).await;

    type_input(&mut app, "aXcme").await;
    app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.query(), "acme");
    app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.query(), "ace");

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    assert!(app.provider_search.query().is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::List);
    assert!(!rendered_cursor_visible(&mut app, 100, 30));
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
    assert!(rendered_cursor_visible(&mut app, 100, 30));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectProviders);
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.provider_search.query().is_empty());

    app.run_command(SlashCommand::Connect).await;
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
}

#[tokio::test]
async fn provider_search_arrows_and_enter_select_a_filtered_provider() {
    let mut app = test_app().await;
    app.providers = vec![
        provider_descriptor("match-first", "supported", "current", false),
        provider_descriptor("excluded", "supported", "current", false),
        provider_descriptor("match-second", "supported", "current", false),
    ];
    app.run_command(SlashCommand::Connect).await;
    type_input(&mut app, "match").await;

    assert_eq!(app.filtered_providers().len(), 2);
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::List);
    assert_eq!(app.picker_state.selected(), Some(0));
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.picker_state.selected(), Some(1));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    assert_eq!(app.modal, Modal::ConnectSetup);
    assert_eq!(
        app.connect_provider
            .as_ref()
            .map(|provider| provider.id.as_str()),
        Some("match-second")
    );
}

#[tokio::test]
async fn credential_inputs_wipe_on_cancel_and_app_drop() {
    let before = credential_wipe_count();
    {
        let mut app = test_app().await;
        app.begin_provider_form(catalog_provider());
        app.modal = Modal::ConnectSetup;
        app.provider_form.as_mut().expect("provider form").secrets[0]
            .input
            .insert_owned("sentinel-secret".to_owned());
        app.clear_connect_secrets();
        assert!(app.provider_form.is_none());
    }
    assert!(credential_wipe_count() > before);
}

#[tokio::test]
async fn connect_form_fields_focus_on_click_and_submit_dispatches() {
    let mut app = test_app().await;
    app.begin_provider_form(multi_auth_provider());
    frame_rows(&mut app, 120, 40);

    // Every rendered control registered a hit: the auth selector, one
    // credential, two setup fields, and both action buttons.
    let fields = app.hit_map.provider_fields.clone();
    assert_eq!(fields.len(), 4);
    let submit = app.hit_map.provider_submit.expect("submit hit");
    let cancel = app.hit_map.provider_cancel.expect("cancel hit");
    // The buttons are compact: sized to their labels, side by side on
    // one row with a gutter between frames, never a panel-wide strip.
    assert_eq!(submit.y, cancel.y);
    assert_eq!(submit.height, cancel.height);
    assert!(
        cancel.x > submit.x + submit.width,
        "gutter: {submit:?} {cancel:?}"
    );
    assert!(submit.width < 20, "submit stays compact: {submit:?}");

    // Hovering a control resolves to its own target.
    let credential = fields
        .iter()
        .find(|hit| hit.focus == ProviderFormFocus::Credential(0))
        .copied()
        .expect("credential hit");
    assert_eq!(
        app.hover_target_at(credential.rect.x + 1, credential.rect.y + 1),
        Some(HoverTarget::ProviderField(ProviderFormFocus::Credential(0)))
    );
    assert_eq!(
        app.hover_target_at(submit.x + 1, submit.y + 1),
        Some(HoverTarget::ProviderSubmit)
    );
    assert_eq!(
        app.hover_target_at(cancel.x + 1, cancel.y + 1),
        Some(HoverTarget::ProviderCancel)
    );

    // Clicking the auth selector cycles the method, mirroring Enter, and
    // wipes the previous method's stale secrets.
    let auth = fields
        .iter()
        .find(|hit| hit.focus == ProviderFormFocus::AuthMethod)
        .copied()
        .expect("auth hit");
    app.handle_click(auth.rect.x + 2, auth.rect.y + 1).await;
    let form = app.provider_form.as_ref().expect("form");
    assert_eq!(form.auth_method.as_str(), "bearer");
    assert!(form.secrets[0].input.as_str().is_empty());

    // Clicking a credential focuses it and places the cursor at the
    // clicked display column of the real (unmasked) buffer.
    app.provider_form.as_mut().expect("form").secrets[0]
        .input
        .insert_owned("hunter2".to_owned());
    // The cycled auth method replaced the secret editors; a fresh frame
    // gives the new editor its render layout before the click maps
    // display cells to a cursor.
    frame_rows(&mut app, 120, 40);
    app.handle_click(credential.text_rect.x + 3, credential.text_rect.y)
        .await;
    let form = app.provider_form.as_mut().expect("form");
    assert_eq!(form.focus(), ProviderFormFocus::Credential(0));
    assert_eq!(form.secrets[0].input.state_mut().cursor_byte(), 3);

    // Clicking submit routes through the same validation as Enter:
    // required setup fields are empty, so the error stays inline in the
    // form — the modal and focus are retained for correction.
    app.handle_click(submit.x + 1, submit.y + 1).await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    assert_eq!(
        app.provider_form.as_ref().expect("form").focus(),
        ProviderFormFocus::Credential(0)
    );
    assert!(app.provider_form.as_ref().expect("form").error.is_some());
}

#[tokio::test]
async fn connect_form_cancel_button_cancels_by_click_focus_and_enter() {
    let before = credential_wipe_count();
    let mut app = test_app().await;
    app.begin_provider_form(multi_auth_provider());
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "cancelled-secret").await;
    frame_rows(&mut app, 120, 40);
    let cancel = app.hit_map.provider_cancel.expect("cancel hit");

    // Clicking Cancel does exactly what Escape does: wipe the secrets,
    // dismiss the modal, and keep the form gone.
    app.handle_click(cancel.x + 1, cancel.y + 1).await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.provider_form.is_none());
    assert!(credential_wipe_count() > before);
    assert!(app.status.contains("cancelled"));

    // Cancel is the final Tab stop, after Submit…
    let mut app = test_app().await;
    app.begin_provider_form(multi_auth_provider());
    let form = app.provider_form.as_mut().expect("form");
    form.set_focus(ProviderFormFocus::Submit);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    assert_eq!(
        app.provider_form.as_ref().expect("form").focus(),
        ProviderFormFocus::Cancel
    );
    // …and Shift-Tab walks back to Submit.
    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .await;
    assert_eq!(
        app.provider_form.as_ref().expect("form").focus(),
        ProviderFormFocus::Submit
    );

    // Enter on the focused Cancel button aborts like Escape, without
    // dispatching a connect.
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.provider_form.is_none());
    assert!(app.provider_operations.is_empty());
}

#[tokio::test]
async fn enter_submits_from_every_focus_and_validation_keeps_the_form_open() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.begin_provider_form(multi_auth_provider());

    // Required values are empty: Enter from every focus position routes
    // to the submit path, surfaces the validation error inline, and
    // leaves the modal, the focus, and the auth method untouched.
    let focuses = [
        ProviderFormFocus::AuthMethod,
        ProviderFormFocus::Credential(0),
        ProviderFormFocus::Setup(0),
        ProviderFormFocus::Setup(1),
        ProviderFormFocus::Submit,
    ];
    for focus in focuses {
        let form = app.provider_form.as_mut().expect("form");
        form.set_focus(focus);
        form.error = None;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let form = app.provider_form.as_ref().expect("form retained");
        assert_eq!(app.modal, Modal::ConnectSetup, "modal stays for {focus:?}");
        assert_eq!(form.focus(), focus, "focus unchanged for {focus:?}");
        assert_eq!(
            form.auth_method.as_str(),
            "api-key",
            "Enter does not cycle the auth method for {focus:?}"
        );
        assert!(
            form.error.is_some(),
            "inline validation error for {focus:?}"
        );
        assert!(
            app.provider_operations.is_empty(),
            "nothing dispatched for {focus:?}"
        );
        let rendered = rendered_frame(&mut app, 120, 40);
        assert!(
            rendered.contains("Region is required"),
            "error renders inline for {focus:?}"
        );
    }
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);

    // With every required value populated, Enter from an input box
    // dispatches exactly like the Submit button.
    let form = app.provider_form.as_mut().expect("form");
    form.secrets[0].input.insert_owned("api-secret".to_owned());
    form.setup[0].input.insert_owned("eu".to_owned());
    form.setup[1].input.insert_owned("derived".to_owned());
    form.set_focus(ProviderFormFocus::Setup(1));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    assert!(matches!(
        app.provider_operations
            .get(&ProviderId::new("multi-auth").expect("provider ID")),
        Some(ProviderOperation::InProgress(ProviderAction::Connect))
    ));
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["method"] == "provider.connect")
        .cloned()
        .expect("connect request");
    assert_eq!(request["params"]["auth_method"], "api-key");
    assert_eq!(request["params"]["auth_values"]["api_key"], "api-secret");
    assert_eq!(request["params"]["setup_values"]["region"], "eu");
    app.abort_connect_work();
    drop(incoming_guard);
}

#[tokio::test]
async fn connect_form_cycles_auth_masks_secrets_traverses_and_submits_selected_method() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.begin_provider_form(multi_auth_provider());

    let form = app.provider_form.as_ref().expect("form");
    assert_eq!(form.focus(), ProviderFormFocus::AuthMethod);
    assert_eq!(form.auth_method.as_str(), "api-key");
    assert_eq!(form.secrets[0].descriptor.id.as_str(), "api_key");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "stale-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
        .await;
    let form = app.provider_form.as_ref().expect("form");
    assert_eq!(form.focus(), ProviderFormFocus::AuthMethod);
    assert_eq!(form.auth_method.as_str(), "bearer");
    assert_eq!(form.secrets[0].descriptor.id.as_str(), "access_token");
    assert!(form.secrets[0].input.as_str().is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "bearer-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    app.handle_paste("東京");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "derived-secret").await;

    let rendered = rendered_frame(&mut app, 160, 42);
    assert!(rendered.contains("Authentication method"));
    assert!(rendered.contains("Bearer token (bearer)"));
    assert!(rendered.contains("Credential:"));
    assert!(rendered.contains("Credentials are verified on first use."));
    assert!(rendered.contains("Setup:"));
    assert!(rendered.contains("service_token"));
    assert!(rendered.contains('•'));
    assert!(!rendered.contains("bearer-secret"));
    assert!(!rendered.contains("derived-secret"));
    assert_eq!(
        app.provider_form.as_ref().expect("form").setup[0]
            .input
            .as_str(),
        "東京"
    );

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(
        app.provider_form.as_ref().expect("form").focus(),
        ProviderFormFocus::Submit
    );
    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        .await;
    assert_eq!(
        app.provider_form.as_ref().expect("form").focus(),
        ProviderFormFocus::Setup(1)
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["method"] == "provider.connect")
        .cloned()
        .expect("connect request");
    assert_eq!(request["params"]["auth_method"], "bearer");
    assert_eq!(
        request["params"]["auth_values"]["access_token"],
        "bearer-secret"
    );
    assert!(request["params"]["auth_values"].get("api_key").is_none());
    assert_eq!(request["params"]["setup_values"]["region"], "東京");
    assert_eq!(
        request["params"]["setup_values"]["service_token"],
        "derived-secret"
    );
    app.abort_connect_work();
    drop(incoming_guard);
}

#[tokio::test]
async fn connect_form_escape_cancels_and_clears_values() {
    let before = credential_wipe_count();
    let mut app = test_app().await;
    app.begin_provider_form(multi_auth_provider());
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "cancelled-secret").await;

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;

    assert_eq!(app.modal, Modal::None);
    assert!(app.provider_form.is_none());
    assert!(credential_wipe_count() > before);
}

#[tokio::test]
async fn connect_rpc_error_is_persistent_and_preserves_public_setup_for_retry() {
    let mut app = test_app().await;
    app.begin_provider_form(multi_auth_provider());
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "temporary-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "東京").await;
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "temporary-setup-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;

    let form = app.provider_form.as_ref().expect("form retained in flight");
    assert_eq!(form.setup[0].input.as_str(), "東京");
    assert!(form.setup[1].input.as_str().is_empty());
    assert!(form.secrets[0].input.as_str().is_empty());
    app.abort_connect_work();

    let full_error = "JSON-RPC -32011: catalog_revision_conflict (provider connect error)";
    app.handle_rpc_update(RpcUpdate::ProviderMutationFinished {
        outcome: ProviderMutationOutcome::Failed {
            provider_id: ProviderId::new("multi-auth").expect("provider ID"),
            action: ProviderAction::Connect,
            error: full_error.into(),
        },
    });

    assert_eq!(app.modal, Modal::ConnectError);
    assert_eq!(
        app.provider_form.as_ref().expect("form").setup[0]
            .input
            .as_str(),
        "東京"
    );
    assert!(app.transient_notices.is_empty());
    let first = rendered_frame(&mut app, 160, 36);
    let second = rendered_frame(&mut app, 160, 36);
    assert!(first.contains(full_error));
    assert!(second.contains(full_error));
    assert!(first.contains("catalog_revision_conflict"));
    assert!(first.contains("No credentials were verified"));

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    assert_eq!(
        app.provider_form.as_ref().expect("form").setup[0]
            .input
            .as_str(),
        "東京"
    );
    assert!(app.provider_form.as_ref().expect("form").error.is_none());
}

#[tokio::test]
async fn provider_picker_filter_matches_id_and_name_only() {
    let mut app = test_app().await;
    app.providers = vec![catalog_provider()];
    app.provider_search.input_mut().set_buffer("ACME-AI".into());
    assert_eq!(app.filtered_providers().len(), 1);
    app.provider_search
        .input_mut()
        .set_buffer("provider".into());
    assert_eq!(app.filtered_providers().len(), 1);
    app.provider_search.input_mut().set_buffer("api_key".into());
    assert!(app.filtered_providers().is_empty());
    app.provider_search.input_mut().set_buffer("unknown".into());
    assert!(app.filtered_providers().is_empty());
}

#[tokio::test]
async fn new_connect_session_resets_provider_filter() {
    let mut app = test_app().await;
    app.providers = vec![catalog_provider()];
    app.provider_search
        .input_mut()
        .set_buffer("stale filter".into());
    app.provider_search.focus_list();
    app.picker_state.select(None);

    app.run_command(SlashCommand::Connect).await;

    assert_eq!(app.modal, Modal::ConnectProviders);
    assert!(app.provider_search.query().is_empty());
    assert_eq!(app.provider_search.focus(), SearchPickerFocus::Input);
    assert_eq!(app.filtered_providers().len(), 1);
    assert_eq!(app.picker_state.selected(), Some(0));
}

#[tokio::test]
async fn empty_runtime_uses_exact_message_buffer_has_no_title_hits_and_blocks_rpcs() {
    let mut app = test_app().await;
    app.runtime = crate::state::RuntimeState::default();
    app.install_initial_runtime(runtime_snapshot(
        "2",
        vec![catalog_provider()],
        Vec::new(),
        Vec::new(),
    ));
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.selected = Some(SessionId::new_v7());

    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains(crate::state::EMPTY_RUNTIME_GUIDANCE));
    assert!(app.hit_map.title_segments.is_empty());

    type_input(&mut app, "ordinary text").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.status, crate::state::EMPTY_RUNTIME_GUIDANCE);
    assert_eq!(app.input.as_str(), "ordinary text");
    app.input.set_buffer(String::new());
    settle_recording().await;
    for method in ["session.create", "run.start", "run.steer"] {
        assert_eq!(recorded_method_count(&recorded, method), 0, "{method}");
    }

    type_input(&mut app, "/connect").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectProviders);
    let rendered = rendered_frame(&mut app, 160, 30);
    assert!(rendered.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
    drop(incoming_guard);
}

#[tokio::test]
async fn provider_picker_renders_all_required_row_states() {
    let mut app = test_app().await;
    let unsupported = provider_descriptor("a-unsupported", "unsupported", "current", false);
    let disconnected = provider_descriptor("b-disconnected", "supported", "current", false);
    let connected = provider_descriptor("c-connected", "supported", "current", true);
    let removed = provider_descriptor("d-removed", "supported", "removed", true);
    let error = provider_descriptor("e-error", "supported", "current", false);
    let progress = provider_descriptor("f-progress", "supported", "current", false);
    let mut authored = provider_descriptor("g-authored", "supported", "current", false);
    authored.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
    authored.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredApiKey;
    let mut quarantined = provider_descriptor("h-quarantined", "supported", "current", false);
    quarantined.support.state = cookie_agent_protocol::ProviderSupportState::Quarantined;
    quarantined.support.reason = Some(SafeCode::new("invalid_recipe").expect("reason"));
    quarantined.quarantine = Some(cookie_agent_protocol::QuarantineDiagnostic {
        code: SafeCode::new("invalid_recipe").expect("code"),
        message: SafeErrorMessage::new("recipe was quarantined").expect("message"),
    });
    app.providers = vec![
        unsupported,
        disconnected,
        connected,
        removed,
        error.clone(),
        progress.clone(),
        authored,
        quarantined,
    ];
    app.provider_operations.insert(
        error.id.clone(),
        ProviderOperation::Error {
            action: ProviderAction::Connect,
            message: "retryable".into(),
        },
    );
    app.provider_operations.insert(
        progress.id.clone(),
        ProviderOperation::InProgress(ProviderAction::Connect),
    );
    app.modal = Modal::ConnectProviders;
    let rendered = rendered_frame(&mut app, 220, 40);
    for text in [
        "unsupported: unsupported_environment",
        "disconnected",
        "connected · Enter: reconnect/update",
        "removed from current catalog",
        "error · Enter: retry · retryable",
        "connect in progress",
        "g-authored provider (g-authored) — disconnected",
        "quarantined: invalid_recipe",
    ] {
        assert!(rendered.contains(text), "missing {text}: {rendered}");
    }
    assert!(rendered.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
}

#[tokio::test]
async fn reconnect_prefills_public_setup_but_keeps_secret_fields_blank() {
    let mut app = test_app().await;
    app.begin_provider_form(provider_descriptor(
        "connected-provider",
        "supported",
        "current",
        true,
    ));
    let form = app.provider_form.as_ref().expect("provider form");
    assert!(form.reconnect);
    assert_eq!(form.setup[0].input.as_str(), "us-east-1");
    assert!(form.secrets[0].input.as_str().is_empty());

    let public = rendered_frame(&mut app, 100, 30);
    assert!(public.contains("Setup:"));
    assert!(public.contains("us-east-1"));
    let secret = rendered_frame(&mut app, 100, 30);
    assert!(secret.contains("Credential:"));
    assert!(secret.contains("Setup:"));
    assert!(secret.contains("us-east-1"));
}

#[tokio::test]
async fn setup_and_secret_validation_fail_before_provider_rpc() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.begin_provider_form(catalog_provider());
    let form = app.provider_form.as_mut().expect("provider form");
    form.setup[0].input.set_buffer("x".into());
    form.secrets[0].input.insert_owned("secret".into());
    app.dispatch_provider_connect();
    assert!(app.status.contains("Invalid public setup"));
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);

    app.begin_provider_form(catalog_provider());
    app.dispatch_provider_connect();
    assert!(app.status.contains("Invalid credentials"));
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);
    drop(incoming_guard);
}

#[tokio::test]
async fn unsupported_enter_is_details_only_and_never_connects() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    app.providers = vec![provider_descriptor(
        "unsupported-provider",
        "unsupported",
        "current",
        false,
    )];
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectDetails);
    let details = rendered_frame(&mut app, 100, 30);
    assert!(details.contains("Typed reason: unsupported_environment"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectDetails);
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 0);
    drop(incoming_guard);
}

#[tokio::test]
async fn supported_removed_provider_reconnects_through_setup_and_secret_workflow() {
    let current_catalog = production_openai_catalog('a', false);
    let harness = production_provider_harness(Arc::clone(&current_catalog), |_| {});
    let client = Client::connect_in_process(Arc::clone(&harness.server));
    client.handshake().await.expect("production handshake");
    let current = client.runtime_snapshot().await.expect("current runtime");
    let mut initial_form = ProviderForm::new(current.snapshot.providers[0].clone(), false)
        .expect("initial provider form");
    initial_form.secrets[0]
        .input
        .insert_owned("stored-secret".to_owned());
    client
        .connect_provider(cookie_agent_protocol::ProviderConnectParams {
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_catalog_revision: current.snapshot.catalog_revision,
            setup_values: initial_form.setup_values().expect("initial setup"),
            auth_method: initial_form.auth_method.clone(),
            auth_values: initial_form.auth_values().expect("initial credentials"),
            client_connect_id: cookie_agent_protocol::ClientConnectId::new(
                "tui-connect-before-removal",
            )
            .expect("connect ID"),
        })
        .await
        .expect("initial provider connect");
    initial_form.wipe_secrets();
    let removed_catalog = production_empty_catalog('b');
    harness
        .engine
        .refresh_catalog(Arc::clone(&removed_catalog))
        .expect("catalog churn");
    let removed = client.runtime_snapshot().await.expect("removed runtime");
    let projected = &removed.snapshot.providers[0];
    assert_eq!(
        projected.presence,
        cookie_agent_protocol::ProviderPresence::Removed
    );
    assert_eq!(
        projected.support.state,
        cookie_agent_protocol::ProviderSupportState::Supported
    );
    assert!(projected.durable_connection.is_some());
    let generation_before = removed.snapshot.provider_store_generation;

    let mut app = test_app().await;
    app.client = client.clone();
    app.runtime = crate::state::RuntimeState::default();
    app.install_initial_runtime(removed.snapshot);
    type_input(&mut app, "/connect").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectProviders);
    app.picker_state.select(Some(0));

    let picker = rendered_frame(&mut app, 160, 36);
    assert!(picker.contains("removed from current catalog · Enter: reconnect/update"));
    assert!(picker.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
    app.provider_search.focus_list();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    let credentials = rendered_frame(&mut app, 160, 36);
    assert!(credentials.contains("Credential:"));
    assert!(credentials.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
    type_input(&mut app, "removed-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    let submit = rendered_frame(&mut app, 140, 32);
    assert!(submit.contains("Reconnect"));
    assert!(submit.contains("Cancel"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    let update = tokio::time::timeout(Duration::from_secs(2), app.rpc_updates_rx.recv())
        .await
        .expect("reconnect update timeout")
        .expect("reconnect update");
    app.handle_rpc_update(update);
    let reconnected = client
        .runtime_snapshot()
        .await
        .expect("reconnected runtime");
    assert!(reconnected.snapshot.provider_store_generation > generation_before);
    let projected = &reconnected.snapshot.providers[0];
    assert_eq!(
        projected.presence,
        cookie_agent_protocol::ProviderPresence::Removed
    );
    assert_eq!(
        projected.support.state,
        cookie_agent_protocol::ProviderSupportState::Supported
    );
    assert_eq!(
        projected.effective_auth_state,
        cookie_agent_protocol::EffectiveAuthState::ProviderStore
    );
    app.abort_connect_work();
}

#[tokio::test]
async fn unsupported_removed_provider_is_typed_details_only() {
    let catalog = production_empty_catalog('c');
    let store_catalog = Arc::clone(&catalog);
    let harness = production_provider_harness(Arc::clone(&catalog), move |store| {
        install_unmatched_openai_connection(store, &store_catalog);
    });
    let client = Client::connect_in_process(Arc::clone(&harness.server));
    client.handshake().await.expect("production handshake");
    let runtime = client.runtime_snapshot().await.expect("unmatched runtime");
    let projected = &runtime.snapshot.providers[0];
    assert_eq!(
        projected.presence,
        cookie_agent_protocol::ProviderPresence::Removed
    );
    assert_eq!(
        projected.support.state,
        cookie_agent_protocol::ProviderSupportState::Unsupported
    );
    assert_eq!(
        projected.support.reason.as_ref().map(SafeCode::as_str),
        Some("removed_without_retained_recipe_match")
    );
    let generation_before = runtime.snapshot.provider_store_generation;

    let mut app = test_app().await;
    app.client = client.clone();
    app.runtime = crate::state::RuntimeState::default();
    app.install_initial_runtime(runtime.snapshot);
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    let picker = rendered_frame(&mut app, 140, 32);
    assert!(picker.contains("removed · unsupported: removed_without_retained_recipe_match"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectDetails);
    let details = rendered_frame(&mut app, 140, 32);
    assert!(details.contains("Presence: Removed"));
    assert!(details.contains("Typed reason: removed_without_retained_recipe_match"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectDetails);
    let after = client.runtime_snapshot().await.expect("unchanged runtime");
    assert_eq!(after.snapshot.provider_store_generation, generation_before);
}

#[tokio::test]
async fn catalog_shape_does_not_quarantine_provider() {
    let catalog = production_openai_catalog('e', true);
    let harness = production_provider_harness(catalog, |_| {});
    let client = Client::connect_in_process(Arc::clone(&harness.server));
    client.handshake().await.expect("production handshake");
    let runtime = client.runtime_snapshot().await.expect("family runtime");
    let projected = &runtime.snapshot.providers[0];
    assert_eq!(
        projected.support.state,
        cookie_agent_protocol::ProviderSupportState::Supported
    );
    assert_eq!(
        projected.support.reason.as_ref().map(SafeCode::as_str),
        None
    );

    let mut app = test_app().await;
    app.client = client.clone();
    app.runtime = crate::state::RuntimeState::default();
    app.install_initial_runtime(runtime.snapshot);
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    let picker = rendered_frame(&mut app, 140, 32);
    assert!(picker.contains("disconnected"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    let after = client.runtime_snapshot().await.expect("unchanged runtime");
    assert_eq!(
        after.snapshot.provider_store_generation,
        app.runtime
            .snapshot()
            .expect("installed runtime")
            .provider_store_generation
    );
}

#[tokio::test]
async fn authored_incomplete_provider_is_disconnected_and_opens_public_setup() {
    let mut app = test_app().await;
    let mut provider = provider_descriptor("authored-incomplete", "supported", "current", false);
    provider.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
    provider.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredApiKey;
    provider.setup_fields[0].default = None;
    app.providers = vec![provider];
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    let picker = rendered_frame(&mut app, 140, 32);
    assert!(picker.contains("authored-incomplete provider (authored-incomplete) — disconnected"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    let setup = rendered_frame(&mut app, 160, 32);
    assert!(setup.contains("Connect provider"));
    assert!(setup.contains(crate::ui::provider::DURABLE_PROVIDER_COPY));
    assert!(
        app.provider_form
            .as_ref()
            .is_some_and(|form| form.setup[0].input.as_str().is_empty())
    );
}

#[tokio::test]
async fn complete_authored_override_is_disconnected_and_can_create_global_connection() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    let mut provider = provider_descriptor("gateway", "supported", "current", false);
    provider.configuration = cookie_agent_protocol::ProviderConfigurationState::Authored;
    provider.effective_auth_state = cookie_agent_protocol::EffectiveAuthState::AuthoredOverride;
    provider.setup_fields[0].default = None;
    app.providers = vec![provider];
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    let picker = rendered_frame(&mut app, 220, 32);
    assert!(picker.contains(
            "gateway provider (gateway) — disconnected · config override active · Enter: create global stored connection"
        ));
    assert!(!picker.contains("https://"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    let form = app.provider_form.as_ref().expect("form");
    assert!(!form.reconnect);
    assert!(!form.can_disconnect);
    let setup = rendered_frame(&mut app, 140, 32);
    assert!(!setup.contains("Ctrl-D disconnect"));
    assert!(!setup.contains("https://"));
    type_input(&mut app, "rotated-authored-secret").await;
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
        .await;
    type_input(&mut app, "us-east-1").await;
    let submit = rendered_frame(&mut app, 140, 32);
    assert!(submit.contains("Connect"));
    assert!(submit.contains("Cancel"));
    assert!(!submit.contains("Reconnect"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
    assert_eq!(recorded_method_count(&recorded, "provider.disconnect"), 0);
    let request = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["method"] == "provider.connect")
        .cloned()
        .expect("connect request");
    assert_eq!(request["params"]["provider_id"], "gateway");
    assert_eq!(request["params"]["setup_values"]["region"], "us-east-1");
    assert_eq!(
        request["params"]["auth_values"]["api_key"],
        "rotated-authored-secret"
    );
    app.abort_connect_work();
    drop(incoming_guard);
}

#[tokio::test]
async fn reconnect_and_disconnect_emit_only_protocol8_provider_methods() {
    let mut app = test_app().await;
    let (client, recorded, incoming_guard) = live_recording_client();
    app.client = client;
    let provider = provider_descriptor("connected-provider", "supported", "current", true);
    app.begin_provider_form(provider.clone());
    app.provider_form.as_mut().expect("form").secrets[0]
        .input
        .insert_owned("sentinel-secret".into());
    app.dispatch_provider_connect();
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.connect"), 1);
    let connect = recorded
        .lock()
        .expect("recorded")
        .iter()
        .find(|value| value["method"] == "provider.connect")
        .cloned()
        .expect("connect request");
    assert_eq!(connect["params"]["setup_values"]["region"], "us-east-1");
    assert_eq!(
        connect["params"]["auth_values"]["api_key"],
        "sentinel-secret"
    );
    app.abort_connect_work();

    app.provider_operations.remove(&provider.id);
    app.providers = vec![provider];
    app.modal = Modal::ConnectProviders;
    app.picker_state.select(Some(0));
    app.provider_search.focus_list();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::ConnectSetup);
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.modal, Modal::DisconnectConfirm);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    settle_recording().await;
    assert_eq!(recorded_method_count(&recorded, "provider.disconnect"), 1);
    for legacy in [
        "model.list",
        "agent.list",
        "catalog.provider.list",
        "catalog.model.list",
    ] {
        assert_eq!(recorded_method_count(&recorded, legacy), 0, "{legacy}");
    }
    app.abort_connect_work();
    drop(incoming_guard);
}

#[tokio::test]
async fn runtime_notifications_are_predecessor_monotonic_and_keep_active_runs_frozen() {
    let mut app = test_app().await;
    let initial = app.runtime.revision().cloned().expect("initial runtime");
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            ..SessionState::default()
        },
    );
    let newer = runtime_snapshot(
        "2",
        vec![catalog_provider()],
        vec![model_descriptor()],
        vec![descriptor("reviewer", true)],
    );
    assert!(
        app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(initial),
            snapshot: newer.clone(),
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::ConfigReloaded],
        })
    );
    let installed = app.runtime.revision().cloned().expect("new runtime");
    let stale = runtime_snapshot("3", Vec::new(), Vec::new(), Vec::new());
    assert!(
        !app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: None,
            snapshot: stale,
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::Startup],
        })
    );
    assert_eq!(app.runtime.revision(), Some(&installed));
    assert_eq!(app.active_run_agent().map(AgentId::as_str), Some("primary"));
}

#[tokio::test]
async fn catalog_refresh_and_fallback_notifications_update_durable_state_monotonically() {
    let mut app = test_app().await;
    let ready_revision = app.runtime.revision().cloned().expect("ready runtime");
    let mut stale = runtime_snapshot(
        "9",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    stale.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
    stale.catalog_state.stale = true;
    stale.catalog_state.last_error = Some(cookie_agent_protocol::CatalogSafeErrorMeta {
        code: SafeCode::new("network_unavailable").expect("code"),
        message: SafeErrorMessage::new("catalog refresh failed").expect("message"),
        time: Timestamp::now(),
    });
    assert!(
        app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(ready_revision.clone()),
            snapshot: stale,
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
        })
    );
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Stale);
    let stale_revision = app.runtime.revision().cloned().expect("stale runtime");
    let stale_buffer = rendered_frame(&mut app, 160, 30);
    assert!(stale_buffer.contains("Using stale catalog cache"));

    let refreshed = runtime_snapshot(
        "a",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    assert!(
        app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(stale_revision),
            snapshot: refreshed,
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed],
        })
    );
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Ready);
    assert!(app.runtime.durable_explanation().is_none());
    let refreshed_revision = app.runtime.revision().cloned().expect("refreshed runtime");

    let mut old_fallback = runtime_snapshot(
        "b",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    old_fallback.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
    old_fallback.catalog_state.stale = true;
    assert!(
        !app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(ready_revision),
            snapshot: old_fallback,
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
        })
    );
    assert_eq!(app.runtime.revision(), Some(&refreshed_revision));
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Ready);

    let mut bootstrap = runtime_snapshot(
        "c",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    bootstrap.catalog_source = cookie_agent_protocol::CatalogSource::Bootstrap;
    bootstrap.catalog_state.stale = true;
    assert!(
        app.install_runtime_notification(cookie_agent_protocol::RuntimeChangedNotification {
            previous_revision: Some(refreshed_revision),
            snapshot: bootstrap,
            reasons: vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback],
        })
    );
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Bootstrap);
    let bootstrap_buffer = rendered_frame(&mut app, 160, 30);
    assert!(bootstrap_buffer.contains("Using bundled bootstrap catalog"));
}

#[tokio::test]
async fn coherent_connect_runtime_restores_draft_or_preserves_empty_exactly() {
    let mut app = test_app().await;
    app.runtime = crate::state::RuntimeState::default();
    app.install_initial_runtime(runtime_snapshot(
        "4",
        vec![catalog_provider()],
        Vec::new(),
        Vec::new(),
    ));
    let baseline = app.runtime.revision().cloned();
    app.apply_provider_mutation_outcome(ProviderMutationOutcome::Connected {
        provider_id: cookie_agent_protocol::ProviderId::new("acme-ai").expect("provider"),
        baseline,
        runtime: Box::new(runtime_snapshot(
            "5",
            vec![provider_descriptor("acme-ai", "supported", "current", true)],
            vec![model_descriptor()],
            vec![descriptor("primary", true)],
        )),
    });
    assert!(app.draft.is_some());
    let ready = rendered_frame(&mut app, 100, 30);
    assert!(ready.contains("primary • gateway/arbitrary-model[base]"));
    assert_eq!(app.hit_map.title_segments.len(), 3);

    let baseline = app.runtime.revision().cloned();
    app.apply_provider_mutation_outcome(ProviderMutationOutcome::Connected {
        provider_id: cookie_agent_protocol::ProviderId::new("acme-ai").expect("provider"),
        baseline,
        runtime: Box::new(runtime_snapshot(
            "6",
            vec![provider_descriptor("acme-ai", "supported", "current", true)],
            Vec::new(),
            Vec::new(),
        )),
    });
    assert!(app.runtime.is_empty());
    assert!(app.draft.is_none());
    assert_eq!(app.status, crate::state::EMPTY_RUNTIME_GUIDANCE);
    let empty = rendered_frame(&mut app, 100, 30);
    assert!(empty.contains(crate::state::EMPTY_RUNTIME_GUIDANCE));
    assert!(app.hit_map.title_segments.is_empty());
}

#[tokio::test]
async fn loading_stale_bootstrap_and_error_retry_have_distinct_durable_ui() {
    let mut app = test_app().await;
    app.runtime = crate::state::RuntimeState::default();
    app.draft = None;
    let loading = rendered_frame(&mut app, 100, 30);
    assert!(loading.contains("loading runtime snapshot"));
    assert!(app.hit_map.title_segments.is_empty());

    let mut stale = runtime_snapshot(
        "7",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    stale.catalog_source = cookie_agent_protocol::CatalogSource::Cache;
    stale.catalog_state.stale = true;
    stale.catalog_state.last_error = Some(cookie_agent_protocol::CatalogSafeErrorMeta {
        code: SafeCode::new("network_unavailable").expect("code"),
        message: SafeErrorMessage::new("catalog refresh failed").expect("message"),
        time: Timestamp::now(),
    });
    app.install_initial_runtime(stale);
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Stale);
    let stale_ui = rendered_frame(&mut app, 160, 30);
    assert!(stale_ui.contains("Using stale catalog cache"));
    app.modal = Modal::Models;
    let stale_modal = rendered_frame(&mut app, 160, 30);
    assert!(stale_modal.contains("Using stale catalog cache"));

    app.runtime = crate::state::RuntimeState::default();
    let mut bootstrap = runtime_snapshot(
        "8",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    );
    bootstrap.catalog_source = cookie_agent_protocol::CatalogSource::Bootstrap;
    bootstrap.catalog_state.stale = true;
    app.install_initial_runtime(bootstrap);
    assert_eq!(app.runtime.phase(), crate::state::RuntimePhase::Bootstrap);
    let bootstrap_ui = rendered_frame(&mut app, 160, 30);
    assert!(bootstrap_ui.contains("Using bundled bootstrap catalog"));

    app.runtime = crate::state::RuntimeState::default();
    app.runtime.set_error("runtime unavailable");
    app.draft = None;
    app.modal = Modal::None;
    let error = rendered_frame(&mut app, 100, 30);
    assert!(error.contains("runtime error — retry"));
    assert!(error.contains("runtime unavailable"));
}
