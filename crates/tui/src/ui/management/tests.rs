use std::collections::BTreeMap;

use cookie_agent_protocol::{
    EffectivePermissionAction, McpConfigSource, McpOAuthDefinition, McpServerDefinition,
    McpServerInfo, McpServerState, ModelUsageRollup, PermissionAction, PermissionEffect,
    PermissionRuleSource, SessionId, SessionPermissionGetResult, SessionTreeUsageResult,
    SessionUsageResult, SkillDescriptor, SkillSource, SkillsListResult, UsageRollup,
};
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn permission_rows_keep_action_defaults_and_sources() {
    let result = SessionPermissionGetResult {
        permissions: vec![EffectivePermissionAction {
            action: PermissionAction::Bash,
            effect: PermissionEffect::Deny,
            source: PermissionRuleSource::SessionOverlay,
            patterns: Vec::new(),
        }],
        current_mode: None,
    };
    assert_eq!(
        super::permission_rows(&result),
        vec![super::PermissionRow {
            action: PermissionAction::Bash,
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
        name: "remote".into(),
        source: McpConfigSource::WorkspaceFile,
        definition: McpServerDefinition {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some("https://example.test/mcp".into()),
            headers: BTreeMap::from([("Authorization".into(), "Bearer test".into())]),
            oauth: Some(McpOAuthDefinition::Bool(true)),
            enabled: true,
            lazy: false,
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
            lazy: false,
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
    assert!(text.contains("c copy URL | esc cancel"), "{text}");

    let mut terminal_state = panel.servers.clone();
    terminal_state[0].auth_in_progress = Some(false);
    terminal_state[0].message = Some("OAuth authorization timed out".into());
    panel.install(terminal_state);
    assert!(panel.auth.is_none());
}

#[test]
fn permission_editor_renders_effect_and_source() {
    let mut panel = super::PermissionPanel::default();
    panel.install(SessionPermissionGetResult {
        permissions: vec![EffectivePermissionAction {
            action: PermissionAction::Write,
            effect: PermissionEffect::Deny,
            source: PermissionRuleSource::SessionOverlay,
            patterns: Vec::new(),
        }],
        current_mode: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_permissions(
                frame,
                frame.area(),
                &mut panel,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render permissions");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("write  *  deny  [session_overlay]"), "{text}");
}

#[test]
fn skills_panel_renders_source_precedence_permission_and_location() {
    let mut panel = super::SkillPanel::default();
    panel.install(SkillsListResult {
        skills: vec![SkillDescriptor {
            name: "release-check".into(),
            description: "Check a release".into(),
            when_to_use: None,
            location: "/workspace/.cookie-agent/skills/release-check/SKILL.md".into(),
            source: SkillSource::Project,
            precedence_winner: true,
            permission_effect: PermissionEffect::Allow,
            visible: true,
            user_invocable: true,
            argument_hint: Some("<tag>".into()),
        }],
    });
    let mut terminal = Terminal::new(TestBackend::new(120, 8)).expect("terminal");
    terminal
        .draw(|frame| {
            super::render_skills(
                frame,
                frame.area(),
                &mut panel,
                &crate::theme::Theme::default(),
            );
        })
        .expect("render skills");
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        text.contains("release-check  Project  Allow  winner"),
        "{text}"
    );
    assert!(text.contains("/workspace/.cookie-agent/skills"), "{text}");
}

#[test]
fn permission_panel_is_noninteractive_while_another_session_loads() {
    let mut panel = super::PermissionPanel::default();
    panel.install(SessionPermissionGetResult {
        permissions: vec![EffectivePermissionAction {
            action: PermissionAction::Write,
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
