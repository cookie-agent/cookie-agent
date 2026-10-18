use std::collections::BTreeMap;

use cookie_agent_protocol::{
    EffectivePermissionAction, McpConfigSource, McpOAuthDefinition, McpServerDefinition,
    McpServerInfo, McpServerState, ModelUsageRollup, PermissionAction, PermissionEffect,
    PermissionRuleSource, SessionId, SessionPermissionGetResult, SessionTreeUsageResult,
    SessionUsageResult, UsageRollup,
};
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn permission_rows_keep_action_defaults_and_sources() {
    let result = SessionPermissionGetResult {
        permissions: vec![EffectivePermissionAction {
            action: PermissionAction::BASH,
            effect: PermissionEffect::Deny,
            source: PermissionRuleSource::SessionOverlay,
            patterns: Vec::new(),
        }],
        current_mode: None,
    };
    assert_eq!(
        super::permission_rows(&result),
        vec![super::PermissionRow {
            action: PermissionAction::BASH,
            resource: "*".into(),
            effect: PermissionEffect::Deny,
            source: PermissionRuleSource::SessionOverlay,
        }]
    );
}

#[test]
fn stdio_mcp_form_accepts_args_environment_and_working_directory() {
    let mut form = super::McpForm::add();
    form.name.set_buffer("local".into());
    form.endpoint.set_buffer("server-command".into());
    form.extras.set_buffer("[\"--stdio\"]".into());
    form.environment.set_buffer("{\"TOKEN\":\"value\"}".into());
    form.cwd.set_buffer("/workspace/tools".into());

    let (name, definition) = form.definition().expect("valid stdio form");
    assert_eq!(name, "local");
    assert_eq!(definition.command.as_deref(), Some("server-command"));
    assert_eq!(definition.args, ["--stdio"]);
    assert_eq!(definition.env["TOKEN"], "value");
    assert_eq!(definition.cwd.as_deref(), Some("/workspace/tools"));
}

#[test]
fn mcp_panel_renders_live_state_and_connection() {
    let mut panel = super::McpPanel::default();
    panel.install(vec![McpServerInfo {
        runtime_id: cookie_agent_protocol::RuntimeId::new("global").unwrap(),
        workspace: None,
        name: "remote".into(),
        source: McpConfigSource::UserFile,
        definition: McpServerDefinition {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some("https://example.test/mcp".into()),
            headers: BTreeMap::from([("Authorization".into(), "Bearer test".into())]),
            oauth: Some(McpOAuthDefinition::Bool(true)),
            enabled: true,
            timeout_ms: None,
        },
        state: McpServerState::Disconnected,
        tool_count: 0,
        message: None,
        auth_in_progress: Some(false),
    }]);
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_mcp(
                frame,
                frame.area(),
                &mut panel,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render MCP panel");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("disconnected"), "{text}");
    assert!(text.contains("Authorization: Bearer test"), "{text}");
}

#[test]
fn mcp_panel_renders_copyable_oauth_wait() {
    let mut panel = super::McpPanel::default();
    panel.install(vec![McpServerInfo {
        runtime_id: cookie_agent_protocol::RuntimeId::new("global").unwrap(),
        workspace: None,
        name: "remote".into(),
        source: McpConfigSource::UserFile,
        definition: McpServerDefinition {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some("https://example.test/mcp".into()),
            headers: BTreeMap::new(),
            oauth: Some(McpOAuthDefinition::Bool(true)),
            enabled: true,
            timeout_ms: None,
        },
        state: McpServerState::NeedsAuth,
        tool_count: 0,
        message: Some("waiting for OAuth browser callback".into()),
        auth_in_progress: Some(true),
    }]);
    panel.auth = Some(super::McpAuthView {
        server: "remote".into(),
        authorization_url: "https://auth.example.test/authorize?state=test".into(),
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_mcp(
                frame,
                frame.area(),
                &mut panel,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render MCP OAuth panel");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("needs_auth"), "{text}");
    assert!(
        text.contains("https://auth.example.test/authorize?state=test"),
        "{text}"
    );
    assert!(
        text.contains("c copy URL | paste redirect | esc cancel"),
        "{text}"
    );

    let mut terminal_state = panel.servers.clone();
    terminal_state[0].auth_in_progress = Some(false);
    terminal_state[0].message = Some("OAuth authorization timed out".into());
    panel.install(terminal_state);
    assert!(panel.auth.is_none());
}

fn render_panel(panel: &mut super::PermissionPanel, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_permissions(
                frame,
                frame.area(),
                panel,
                Some(cookie_agent_protocol::PermissionMode::Ask),
                &crate::theme::Theme::default(),
            );
        })
        .expect("render permissions");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn permission_table_groups_rules_by_action_and_explains_the_selection() {
    let mut panel = super::PermissionPanel::default();
    panel.install(super::permissions::sample_permissions());
    // The bash `git status*` rule.
    panel.selection.select(Some(7));
    let text = render_panel(&mut panel, 110, 34);
    for needle in [
        "Permissions",
        "mode ask · m",
        "ACTION",
        "SOURCE",
        "├ *.env",
        "└ rm -rf *",
        "✗ deny",
        "? ask",
        "session",
        "default",
        // The selected row's effect is a segmented control.
        " allow │ ask │ deny ",
        "Bash commands matching `git status*` run without asking.",
        "changing it adds a session override",
        "+ new session rule",
    ] {
        assert!(text.contains(needle), "missing {needle}:\n{text}");
    }
    // Patterns sit beside their action's `*` row, not under a new label.
    assert_eq!(text.matches("bash").count(), 1, "{text}");
}

#[test]
fn permission_panel_shows_loading_until_the_result_lands() {
    let mut panel = super::PermissionPanel::default();
    let text = render_panel(&mut panel, 100, 30);
    assert!(text.contains("Loading permissions…"), "{text}");
    assert_eq!(panel.row_count(), 0);
}

#[test]
fn permission_form_offers_every_action_and_effects_stop_at_the_ends() {
    // The actions the daemon reports, including a runtime's own action.
    let mut actions = PermissionAction::BUILT_IN.to_vec();
    actions.push(PermissionAction::new("browse").unwrap());
    let mut form = super::PermissionForm::new(PermissionAction::READ);
    let mut seen = vec![form.action.clone()];
    for _ in 1..actions.len() {
        form.cycle_action(&actions, false);
        seen.push(form.action.clone());
    }
    assert_eq!(seen, actions);
    form.cycle_action(&actions, false);
    assert_eq!(form.action, PermissionAction::READ);
    form.cycle_action(&actions, true);
    assert_eq!(form.action.as_str(), "browse");

    assert_eq!(
        super::step_effect(PermissionEffect::Allow, true),
        PermissionEffect::Allow
    );
    assert_eq!(
        super::step_effect(PermissionEffect::Allow, false),
        PermissionEffect::Ask
    );
    assert_eq!(
        super::step_effect(PermissionEffect::Deny, false),
        PermissionEffect::Deny
    );
}

#[test]
fn install_keeps_the_selection_on_the_same_rule() {
    let mut panel = super::PermissionPanel::default();
    panel.install(super::permissions::sample_permissions());
    panel.selection.select(Some(7));
    let mut changed = super::permissions::sample_permissions();
    // A new read pattern shifts every later row down by one.
    changed.permissions[0]
        .patterns
        .push(cookie_agent_protocol::EffectivePermissionRule {
            runtime: None,
            resource: cookie_agent_protocol::WildcardPattern::new("src/**").unwrap(),
            effect: PermissionEffect::Allow,
            source: PermissionRuleSource::SessionOverlay,
        });
    panel.install(changed);
    assert_eq!(panel.selected().expect("row").resource, "git status*");
}

#[test]
fn permission_panel_is_noninteractive_while_another_session_loads() {
    let mut panel = super::PermissionPanel::default();
    panel.install(SessionPermissionGetResult {
        permissions: vec![EffectivePermissionAction {
            action: PermissionAction::WRITE,
            effect: PermissionEffect::Deny,
            source: PermissionRuleSource::SessionOverlay,
            patterns: Vec::new(),
        }],
        current_mode: None,
    });
    assert!(!panel.rows().is_empty());

    panel.begin_load();

    assert!(panel.rows().is_empty());
    assert!(panel.selected().is_none());
}

#[test]
fn usage_panel_renders_both_sections_sorted_models_and_unpriced_values() {
    let session_id = SessionId::new_v7();
    let session_usage = UsageRollup {
        input_tokens: 1_000,
        output_tokens: 200,
        reasoning_tokens: 50,
        cache_read_tokens: 500,
        cache_write_tokens: 100,
        request_count: 3,
        cache_hit_rate: Some(0.5),
        estimated_cost_usd: None,
        by_model: BTreeMap::from([
            (
                "test/unpriced".parse().unwrap(),
                ModelUsageRollup {
                    input_tokens: 2_000,
                    request_count: 1,
                    cache_hit_rate: None,
                    estimated_cost_usd: None,
                    ..ModelUsageRollup::default()
                },
            ),
            (
                "test/expensive".parse().unwrap(),
                ModelUsageRollup {
                    input_tokens: 100,
                    request_count: 1,
                    cache_hit_rate: Some(0.0),
                    estimated_cost_usd: Some(0.2),
                    ..ModelUsageRollup::default()
                },
            ),
            (
                "test/cheap".parse().unwrap(),
                ModelUsageRollup {
                    input_tokens: 3_000,
                    request_count: 1,
                    cache_hit_rate: Some(0.5),
                    estimated_cost_usd: Some(0.01),
                    ..ModelUsageRollup::default()
                },
            ),
        ]),
        ..UsageRollup::default()
    };
    let tree_usage = UsageRollup {
        input_tokens: 4_000,
        output_tokens: 800,
        reasoning_tokens: 100,
        cache_read_tokens: 1_500,
        cache_write_tokens: 200,
        request_count: 6,
        cache_hit_rate: Some(0.375),
        estimated_cost_usd: Some(0.21),
        by_model: BTreeMap::from([(
            "test/tree".parse().unwrap(),
            ModelUsageRollup {
                input_tokens: 1_000,
                output_tokens: 200,
                reasoning_tokens: 50,
                cache_read_tokens: 500,
                cache_write_tokens: 100,
                request_count: 3,
                cache_hit_rate: Some(0.5),
                estimated_cost_usd: Some(0.21),
                ..ModelUsageRollup::default()
            },
        )]),
        ..UsageRollup::default()
    };
    let panel = super::UsagePanel {
        session: Some(SessionUsageResult {
            session_id,
            usage: session_usage,
        }),
        tree: Some(SessionTreeUsageResult {
            session_id,
            usage: tree_usage,
            session_count: 3,
        }),
        loading: false,
        ..super::UsagePanel::default()
    };
    let lines = super::usage_panel_lines(&panel, 100, &crate::theme::Theme::default());
    let text = lines
        .iter()
        .map(ratatui::text::Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("This session"), "{text}");
    assert!(text.contains("Session tree · 3 sessions"), "{text}");
    assert!(text.contains("1,000"), "{text}");
    assert!(text.contains("n/a"), "{text}");
    assert!(text.contains("unpriced"), "{text}");
    let expensive = text.find("test/expensive").expect("expensive model");
    let cheap = text.find("test/cheap").expect("cheap model");
    let unpriced = text.find("test/unpriced").expect("unpriced model");
    assert!(expensive < cheap && cheap < unpriced, "{text}");
}

#[test]
fn usage_counts_are_grouped_before_widths_are_computed() {
    let session_id = SessionId::new_v7();
    let usage = UsageRollup {
        request_count: 12_345,
        by_model: BTreeMap::from([(
            "test/counts".parse().unwrap(),
            ModelUsageRollup {
                request_count: 1_234_567,
                ..ModelUsageRollup::default()
            },
        )]),
        ..UsageRollup::default()
    };
    let panel = super::UsagePanel {
        session: Some(SessionUsageResult {
            session_id,
            usage: usage.clone(),
        }),
        tree: Some(SessionTreeUsageResult {
            session_id,
            usage: usage.clone(),
            session_count: 12_345,
        }),
        ..super::UsagePanel::default()
    };
    let text = super::usage_panel_lines(&panel, 100, &crate::theme::Theme::default())
        .iter()
        .map(ratatui::text::Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("· 12,345 sessions"), "{text}");
    assert!(text.contains("12,345"), "{text}");
    assert!(text.contains("1,234,567"), "{text}");
    let widths = super::TableWidths::from_usages([Some(&usage), None]);
    assert_eq!(widths.req, "1,234,567".len());
}

#[test]
fn usage_model_sort_ties_use_input_then_name() {
    let model = |input_tokens| ModelUsageRollup {
        input_tokens,
        request_count: 1,
        estimated_cost_usd: Some(1.0),
        ..ModelUsageRollup::default()
    };
    let usage = UsageRollup {
        request_count: 3,
        by_model: BTreeMap::from([
            ("test/zeta".parse().unwrap(), model(200)),
            ("test/alpha".parse().unwrap(), model(200)),
            ("test/middle".parse().unwrap(), model(300)),
        ]),
        ..UsageRollup::default()
    };
    let text = super::TableWidths::from_usages([Some(&usage), None])
        .lines(&usage, 100, &crate::theme::Theme::default())
        .iter()
        .map(ratatui::text::Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let middle = text.find("test/middle").unwrap();
    let alpha = text.find("test/alpha").unwrap();
    let zeta = text.find("test/zeta").unwrap();
    assert!(middle < alpha && alpha < zeta, "{text}");
}

#[test]
fn usage_panel_degenerate_tree_replaces_totals_with_note() {
    let session_id = SessionId::new_v7();
    let usage = UsageRollup {
        request_count: 1,
        ..UsageRollup::default()
    };
    let panel = super::UsagePanel {
        session: Some(SessionUsageResult {
            session_id,
            usage: usage.clone(),
        }),
        tree: Some(SessionTreeUsageResult {
            session_id,
            usage,
            session_count: 1,
        }),
        ..super::UsagePanel::default()
    };
    let text = super::usage_panel_lines(&panel, 80, &crate::theme::Theme::default())
        .iter()
        .map(ratatui::text::Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Session tree · 1 sessions"), "{text}");
    assert!(
        text.contains("No delegated sessions — totals match this session."),
        "{text}"
    );
}

#[test]
fn usage_table_drops_every_column_in_the_locked_narrow_order() {
    let usage = UsageRollup {
        request_count: 1,
        by_model: BTreeMap::from([(
            "test/a-model-name".parse().unwrap(),
            ModelUsageRollup {
                request_count: 12_345,
                input_tokens: 1_000,
                output_tokens: 2_000,
                reasoning_tokens: 3_000,
                cache_hit_rate: Some(0.5),
                estimated_cost_usd: None,
                ..ModelUsageRollup::default()
            },
        )]),
        ..UsageRollup::default()
    };
    let widths = super::TableWidths::from_usages([Some(&usage), None]);
    let column_widths = [
        widths.req,
        widths.input,
        widths.output,
        widths.reasoning,
        widths.hit,
    ];
    let fixed_width = |visible: [bool; 5]| {
        2 + 2
            + widths.cost
            + visible
                .into_iter()
                .zip(column_widths)
                .filter_map(|(visible, width)| visible.then_some(2 + width))
                .sum::<usize>()
    };
    let transitions = [
        [true, true, true, true, true],
        [true, true, true, false, true],
        [true, true, true, false, false],
        [true, true, false, false, false],
        [false, true, false, false, false],
        [false, false, false, false, false],
    ];
    for expected in transitions {
        let columns = widths.columns(fixed_width(expected) + 16);
        assert_eq!(columns.visible, expected);
        assert_eq!(columns.model, 16);
    }
    let floor = widths.columns(fixed_width([false; 5]) + 8);
    assert_eq!(floor.visible, [false; 5]);
    assert_eq!(floor.model, 8);
}

#[test]
fn usage_panel_footer_switches_when_content_scrolls() {
    let session_id = SessionId::new_v7();
    let usage = UsageRollup {
        request_count: 1,
        ..UsageRollup::default()
    };
    let mut fitting = super::UsagePanel {
        tree: Some(SessionTreeUsageResult {
            session_id,
            usage: usage.clone(),
            session_count: 1,
        }),
        session: Some(SessionUsageResult { session_id, usage }),
        ..super::UsagePanel::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_usage(
                frame,
                frame.area(),
                &mut fitting,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render fitting usage");
    let fitting_text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(fitting_text.contains("esc close"), "{fitting_text}");
    assert!(!fitting_text.contains("arrows scroll"), "{fitting_text}");

    let mut terminal = Terminal::new(TestBackend::new(50, 8)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_usage(
                frame,
                frame.area(),
                &mut fitting,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render scrolling usage");
    let scrolling_text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        scrolling_text.contains("arrows scroll | esc close"),
        "{scrolling_text}"
    );
    let buffer = terminal.backend().buffer();
    for y in 1..7 {
        assert_eq!(buffer[(47, y)].symbol(), " ", "reserved gutter at row {y}");
        assert!(
            matches!(buffer[(48, y)].symbol(), "│" | "█"),
            "scrollbar at row {y}: {}",
            buffer[(48, y)].symbol()
        );
    }
}

fn capability(
    name: &str,
    kind: &str,
    takes_workspace: bool,
) -> cookie_agent_protocol::RuntimeCapabilityInfo {
    cookie_agent_protocol::RuntimeCapabilityInfo {
        name: cookie_agent_protocol::CapabilityName::new(name).unwrap(),
        kind: cookie_agent_protocol::RuntimeKind::new(kind).unwrap(),
        takes_workspace,
        description: String::new(),
    }
}

fn attachment(
    name: &str,
    runtime_id: &str,
    capability: &str,
    workspace: Option<&str>,
) -> cookie_agent_protocol::RuntimeAttachment {
    cookie_agent_protocol::RuntimeAttachment {
        name: cookie_agent_protocol::AttachmentName::new(name).unwrap(),
        runtime_id: cookie_agent_protocol::RuntimeId::new(runtime_id).unwrap(),
        capability: cookie_agent_protocol::CapabilityName::new(capability).unwrap(),
        kind: cookie_agent_protocol::RuntimeKind::new(capability.replace('_', "-")).unwrap(),
        workspace: workspace.map(str::to_owned),
    }
}

/// `global`; `labpc`, which has not connected yet; and `devbox`, which
/// offers `coding` and `computer_use`.
fn runtime_listing() -> cookie_agent_protocol::RuntimesListResult {
    use cookie_agent_protocol::{
        AttachmentName, CapabilityName, RuntimeId, RuntimeInstanceInfo, RuntimeInstanceStatus,
        RuntimeKind, RuntimeLifetime, RuntimeListing,
    };
    let instance =
        |id: &str, name: &str, workspace: Option<&str>, status, message: Option<&str>| {
            let capability = if id == "global" { "global" } else { "coding" };
            RuntimeInstanceInfo {
                runtime_id: RuntimeId::new(id).unwrap(),
                capability: CapabilityName::new(capability).unwrap(),
                attachment_name: Some(AttachmentName::new(name).unwrap()),
                kind: Some(RuntimeKind::new(capability).unwrap()),
                workspace: workspace.map(str::to_owned),
                platform: workspace.map(|_| "linux".to_owned()),
                status,
                message: message.map(str::to_owned),
            }
        };
    let listing = |id: &str, lifetime, connected, capabilities| RuntimeListing {
        runtime_id: RuntimeId::new(id).unwrap(),
        lifetime,
        connected,
        capabilities,
        platform: None,
        message: None,
    };
    cookie_agent_protocol::RuntimesListResult {
        runtimes: vec![
            listing(
                "global",
                RuntimeLifetime::Builtin,
                true,
                vec![capability("global", "global", false)],
            ),
            listing("labpc", RuntimeLifetime::Spawned, false, Vec::new()),
            listing(
                "devbox",
                RuntimeLifetime::Spawned,
                true,
                vec![
                    capability("coding", "coding", true),
                    capability("computer_use", "desktop", false),
                ],
            ),
        ],
        instances: vec![
            instance("global", "global", None, RuntimeInstanceStatus::Ready, None),
            instance(
                "local",
                "local_web",
                Some("/srv/web"),
                RuntimeInstanceStatus::Ready,
                None,
            ),
            instance(
                "labpc",
                "labpc_api",
                Some("/srv/api"),
                RuntimeInstanceStatus::Failed,
                Some("runtime `labpc` failed to start: connection refused"),
            ),
        ],
    }
}

#[test]
fn runtime_rows_match_instances_and_read_legacy_sessions_as_global_and_local() {
    let listing = runtime_listing();
    let attached = [attachment("labpc_api", "labpc", "coding", Some("/srv/api"))];
    let rows = super::runtime_rows(Some(&attached), Some(&listing));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].instance.as_ref().map(|instance| instance.status),
        Some(cookie_agent_protocol::RuntimeInstanceStatus::Failed)
    );
    // Another capability of the same runtime is another instance.
    let other = [attachment(
        "labpc_api",
        "labpc",
        "computer_use",
        Some("/srv/api"),
    )];
    assert!(
        super::runtime_rows(Some(&other), Some(&listing))[0]
            .instance
            .is_none()
    );

    let legacy = super::runtime_rows(None, Some(&listing));
    assert_eq!(
        legacy
            .iter()
            .map(|row| (
                row.attachment.name.as_str(),
                row.attachment.capability.as_str()
            ))
            .collect::<Vec<_>>(),
        [("global", "global"), ("local_web", "coding")]
    );
    assert!(legacy.iter().all(|row| row.instance.is_some()));
    assert!(super::runtime_rows(Some(&[]), Some(&listing)).is_empty());
}

#[test]
fn runtime_attach_lists_every_capability_and_asks_for_parameters_only_when_needed() {
    let mut panel = super::RuntimePanel::default();
    assert!(panel.begin_attach().is_err(), "nothing to attach yet");
    panel.install(runtime_listing());
    panel.begin_attach().expect("attach step");
    let labels = |panel: &super::RuntimePanel| {
        panel
            .choices()
            .iter()
            .map(|choice| {
                choice.capability.as_ref().map_or_else(
                    || choice.runtime_id.to_string(),
                    |capability| format!("{}/{}", choice.runtime_id, capability.name),
                )
            })
            .collect::<Vec<_>>()
    };
    // Every capability of every runtime; one that has not connected is one
    // choice.
    assert_eq!(
        labels(&panel),
        [
            "global/global",
            "labpc",
            "devbox/coding",
            "devbox/computer_use"
        ]
    );

    // A capability without a workspace attaches at once.
    match panel.choose().expect("global") {
        super::AttachStep::Attach(runtime_id, capability, workspace) => {
            assert_eq!(runtime_id.as_str(), "global");
            assert_eq!(
                capability.as_ref().map(|name| name.as_str()),
                Some("global")
            );
            assert_eq!(workspace, None);
        }
        super::AttachStep::Parameters => panic!("global takes no parameters"),
    }

    // The search filters by runtime, capability, and kind.
    let form = panel.form.as_mut().expect("form");
    form.search.set_buffer("DESK".into());
    panel.clamp_choice();
    assert_eq!(labels(&panel), ["devbox/computer_use"]);
    panel
        .form
        .as_mut()
        .expect("form")
        .search
        .set_buffer("devbox".into());
    panel.clamp_choice();
    assert_eq!(labels(&panel), ["devbox/coding", "devbox/computer_use"]);

    // A capability that takes a workspace asks for it, and requires it.
    assert!(matches!(
        panel.choose().expect("devbox coding"),
        super::AttachStep::Parameters
    ));
    let error = panel
        .attach_request()
        .expect_err("coding needs a workspace");
    assert!(error.contains("coding"), "{error}");
    panel
        .form
        .as_mut()
        .expect("form")
        .workspace
        .set_buffer(" ~/api ".into());
    let (runtime_id, capability, workspace) = panel.attach_request().expect("devbox coding");
    assert_eq!(runtime_id.as_str(), "devbox");
    assert_eq!(
        capability.as_ref().map(|name| name.as_str()),
        Some("coding")
    );
    assert_eq!(workspace.as_deref(), Some("~/api"));

    // Esc goes back to the list with the search kept, then closes.
    panel.back();
    assert_eq!(labels(&panel).len(), 2);
    assert!(
        panel
            .form
            .as_ref()
            .is_some_and(|form| form.chosen.is_none())
    );

    // A runtime that has not connected: its only capability, and an
    // optional workspace.
    panel
        .form
        .as_mut()
        .expect("form")
        .search
        .set_buffer("labpc".into());
    panel.clamp_choice();
    assert!(matches!(
        panel.choose().expect("labpc"),
        super::AttachStep::Parameters
    ));
    assert_eq!(
        panel.attach_request().expect("labpc without a workspace"),
        (
            cookie_agent_protocol::RuntimeId::new("labpc").unwrap(),
            None,
            None
        )
    );
    panel.back();
    panel.back();
    assert!(panel.form.is_none());
}

#[test]
fn runtime_retry_names_the_attachments_capability_and_workspace() {
    let attached = [
        attachment("global", "global", "global", None),
        attachment("labpc_api", "labpc", "coding", Some("/srv/api")),
    ];
    let rows = super::runtime_rows(Some(&attached), Some(&runtime_listing()));
    let mut panel = super::RuntimePanel::default();
    assert!(
        panel.reconnect_request(&rows[1]).is_err(),
        "no runtimes yet"
    );
    panel.install(runtime_listing());
    let (runtime_id, capability, workspace) = panel.reconnect_request(&rows[1]).expect("labpc");
    assert_eq!(runtime_id.as_str(), "labpc");
    assert_eq!(capability.as_str(), "coding");
    assert_eq!(workspace.as_deref(), Some("/srv/api"));
    let (runtime_id, capability, workspace) = panel.reconnect_request(&rows[0]).expect("global");
    assert_eq!(runtime_id.as_str(), "global");
    assert_eq!(capability.as_str(), "global");
    assert_eq!(workspace, None);
}

#[test]
fn runtime_panel_renders_attachments_with_status_and_failure_reason() {
    let mut listing = runtime_listing();
    listing.runtimes[0].capabilities[0].description = "The daemon's global tools".into();
    let attached = [
        attachment("global", "global", "global", None),
        attachment("labpc_api", "labpc", "coding", Some("/srv/api")),
    ];
    let rows = super::runtime_rows(Some(&attached), Some(&listing));
    let mut panel = super::RuntimePanel::default();
    panel.install(listing);
    panel.selection.select(Some(1));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_runtimes(
                frame,
                frame.area(),
                &mut panel,
                &rows,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render runtime panel");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        text.contains("global  global/global  (no workspace)  ready"),
        "{text}"
    );
    assert!(
        text.contains("labpc_api  labpc/coding  /srv/api  failed"),
        "{text}"
    );
    assert!(text.contains("connection refused"), "{text}");
    assert!(text.contains("a attach | d detach"), "{text}");
    assert_eq!(
        rows[0].description.as_deref(),
        Some("The daemon's global tools")
    );
    assert_eq!(rows[1].description, None);

    // The selected attachment's description shows in its details.
    panel.selection.select(Some(0));
    terminal
        .draw(|frame| {
            super::render_runtimes(
                frame,
                frame.area(),
                &mut panel,
                &rows,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render runtime panel");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        text.contains("description: The daemon's global tools"),
        "{text}"
    );
}

#[test]
fn mcp_panel_lists_runtime_hosted_servers_by_instance_after_global_ones() {
    let server = |runtime: &str, workspace: Option<&str>, name: &str| McpServerInfo {
        runtime_id: cookie_agent_protocol::RuntimeId::new(runtime).unwrap(),
        workspace: workspace.map(str::to_owned),
        name: name.into(),
        source: McpConfigSource::UserFile,
        definition: McpServerDefinition {
            command: Some("lsp-mcp".into()),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            oauth: None,
            enabled: true,
            timeout_ms: None,
        },
        state: McpServerState::Failed,
        tool_count: 0,
        message: Some("exited".into()),
        auth_in_progress: None,
    };
    let mut panel = super::McpPanel::default();
    panel.install(vec![
        server("labpc", Some("/srv/web"), "lsp"),
        server("labpc", Some("/srv/api"), "lsp"),
        server("global", None, "docs"),
    ]);
    let order = panel
        .servers
        .iter()
        .map(|server| (server.runtime_id.to_string(), server.workspace.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        order,
        [
            ("global".to_owned(), None),
            ("labpc".to_owned(), Some("/srv/api".to_owned())),
            ("labpc".to_owned(), Some("/srv/web".to_owned())),
        ]
    );
    panel.selection.select(Some(1));
    assert!(
        panel
            .selected_editable()
            .is_err_and(|refusal| refusal.contains("[runtimes.labpc.mcp]"))
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_mcp(
                frame,
                frame.area(),
                &mut panel,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render MCP panel");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("labpc at /srv/api"), "{text}");
    assert!(text.contains("labpc at /srv/web"), "{text}");
}
