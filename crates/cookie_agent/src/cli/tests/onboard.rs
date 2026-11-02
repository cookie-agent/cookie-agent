//! `cookie onboard`.

use super::*;

/// `cookie onboard` asks for the models, preselects their variants, skips
/// the optional ones on Enter, and sets them through the daemon.
#[tokio::test]
async fn cookie_onboard_sets_the_models() {
    use crate::cli::onboard::{OnboardArgs, run_onboard_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let (client, root) = onboard_daemon(temporary.path()).await;

    // --yes without the default model refuses rather than guess.
    let refused = run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            ..OnboardArgs::default()
        },
        &mut ScriptedConnectIo::default(),
    )
    .await
    .expect_err("refused");
    assert!(
        format!("{refused:#}").contains("--default-model"),
        "{refused:#}"
    );

    // Interactively: model providers (Enter), keep the provider, pick A Model (its default variant
    // preselected) and no fallback, skip the small model, then search for
    // Z Model as the utility model.
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(
            ["", "n", "1", "", "", "", "/z-model", "1", ""]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..ScriptedConnectIo::default()
    };
    run_onboard_with(&client, OnboardArgs::default(), &mut io)
        .await
        .expect("onboard");
    assert!(
        io.output
            .iter()
            .any(|line| line.contains("1. high (Enter)")),
        "the default model preselects its default variant: {:?}",
        io.output
    );
    let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        config.contains("default_model = \"custom.test/a-model\""),
        "{config}"
    );
    assert!(
        config.contains("utility_model = \"custom.test/z-model\""),
        "{config}"
    );
    assert!(!config.contains("small_model"), "{config}");

    // Flags answer the questions; the small model's variant is given.
    let mut io = ScriptedConnectIo::default();
    run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            small_model: vec!["custom.test/a-model:low".into()],
            ..OnboardArgs::default()
        },
        &mut io,
    )
    .await
    .expect("flags");
    assert!(
        io.output
            .iter()
            .any(|line| line == "Set small_model to custom.test/a-model (low)"),
        "{:?}",
        io.output
    );

    // Repeated flags make a fallback list; a field's name refers to it.
    let mut io = ScriptedConnectIo::default();
    run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            default_model: vec!["custom.test/z-model".into(), "custom.test/a-model".into()],
            small_model: vec!["default_model".into()],
            ..OnboardArgs::default()
        },
        &mut io,
    )
    .await
    .expect("lists");
    for line in [
        "Set default_model to custom.test/z-model, then custom.test/a-model",
        "Set small_model to same as default_model",
    ] {
        assert!(
            io.output.iter().any(|output| output == line),
            "{line}: {:?}",
            io.output
        );
    }
    let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        config.contains("small_model = \"${default_model}\""),
        "{config}"
    );

    // Interactively, `d` makes the small model the same as the default
    // model, then a fallback follows it.
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(
            ["", "n", "", "d", "/z-model", "1", "", ""]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..ScriptedConnectIo::default()
    };
    run_onboard_with(&client, OnboardArgs::default(), &mut io)
        .await
        .expect("onboard");
    assert!(
        io.output.iter().any(
            |line| line == "Set small_model to same as default_model, then custom.test/z-model"
        ),
        "{:?}",
        io.output
    );
}

/// `cookie onboard --acp` links an ACP agent without any model, adds no
/// MCP server it was not asked for, and sets the default agent.
#[tokio::test]
async fn cookie_onboard_links_an_acp_agent() {
    use crate::cli::onboard::{OnboardArgs, run_onboard_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let (client, root) = onboard_daemon(temporary.path()).await;
    // An unknown preset is refused, naming the ones there are.
    let refused = run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            acp: Some("nope".into()),
            ..OnboardArgs::default()
        },
        &mut ScriptedConnectIo::default(),
    )
    .await
    .expect_err("refused");
    assert!(format!("{refused:#}").contains("`nope`"), "{refused:#}");

    let mut io = ScriptedConnectIo::default();
    run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            acp: Some("other".into()),
            acp_id: Some("gemini".into()),
            acp_command: Some("gemini --experimental-acp".into()),
            acp_env: vec!["MODE=v2".into()],
            default_agent: Some("gemini".into()),
            ..OnboardArgs::default()
        },
        &mut io,
    )
    .await
    .expect("linked");
    let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        config.contains("[runtimes.local.acp.agents.gemini]"),
        "{config}"
    );
    assert!(config.contains("--experimental-acp"), "{config}");
    assert!(config.contains("default = \"gemini\""), "{config}");
    assert!(
        io.output
            .iter()
            .any(|line| line.contains("Added ACP agent `gemini`")),
        "{:?}",
        io.output
    );
}
