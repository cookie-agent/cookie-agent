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

    // Interactively: keep the provider, pick A Model (its default variant
    // preselected), skip the small model, then search for Z Model as the
    // utility model.
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(
            ["n", "1", "", "", "/z-model", "1"]
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
            small_model: Some("custom.test/a-model:low".into()),
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
}
