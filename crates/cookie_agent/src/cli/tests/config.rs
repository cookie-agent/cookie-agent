//! `cookie config`.

use super::*;

struct ScriptedConfigIo {
    edits: VecDeque<String>,
    confirmations: VecDeque<bool>,
    seen: Vec<String>,
    output: Vec<String>,
}

impl crate::cli::config::ConfigIo for ScriptedConfigIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.output.push(line.to_owned());
        Ok(())
    }

    fn edit(&mut self, _path: &str, text: &str) -> anyhow::Result<String> {
        self.seen.push(text.to_owned());
        self.edits.pop_front().context("missing scripted edit")
    }

    fn confirm(&mut self, _prompt: &str) -> anyhow::Result<bool> {
        self.confirmations
            .pop_front()
            .context("missing scripted confirmation")
    }
}

/// `cookie config` edits, sets, removes, and reloads the configuration of
/// a running daemon; an edit that does not validate is offered again with
/// the text as it was left.
#[tokio::test]
async fn cookie_config_edits_and_sets_the_daemon_configuration() {
    use crate::cli::config::{ConfigCommand, run_config_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol_transport::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let mut io = ScriptedConfigIo {
        edits: VecDeque::from([
            "[providers]\n[unknown]\n".to_owned(),
            "[providers]\n\n[loop_warning]\nenabled = false\n".to_owned(),
        ]),
        confirmations: VecDeque::from([true]),
        seen: Vec::new(),
        output: Vec::new(),
    };
    run_config_with(&client, ConfigCommand::Edit { path: None }, &mut io)
        .await
        .expect("edit");
    // The second round starts from the rejected text.
    assert_eq!(io.seen, ["[providers]\n", "[providers]\n[unknown]\n"]);
    assert!(io.output.iter().any(|line| line.starts_with("Not applied")));
    assert!(!runtime.engine.configuration().runtime.loop_warning.enabled);

    run_config_with(
        &client,
        ConfigCommand::Set {
            key: "model_retry.standard_retries".into(),
            value: "1".into(),
        },
        &mut io,
    )
    .await
    .expect("set");
    assert_eq!(
        runtime
            .engine
            .configuration()
            .runtime
            .model_retry
            .standard_retries,
        1
    );
    run_config_with(
        &client,
        ConfigCommand::Unset {
            key: "loop_warning".into(),
        },
        &mut io,
    )
    .await
    .expect("unset");
    assert!(runtime.engine.configuration().runtime.loop_warning.enabled);
    assert!(
        run_config_with(
            &client,
            ConfigCommand::Set {
                key: "loop_warning.enabled".into(),
                value: "\"not a boolean\"".into(),
            },
            &mut io,
        )
        .await
        .is_err()
    );

    // A new agent document starts from a template and is removed again.
    let agent = "---\ndescription: Reviewer\nmode: subagent\nenabled: true\nmodels: [{ model: \"openai/gpt-test\" }]\npermissions: {}\n---\nReview.\n";
    io.edits.push_back(agent.to_owned());
    run_config_with(
        &client,
        ConfigCommand::Edit {
            path: Some("agents/reviewer.md".into()),
        },
        &mut io,
    )
    .await
    .expect("create an agent");
    assert!(io.seen.last().unwrap().starts_with("---\ndescription:"));
    assert!(workspace.join(".cookie-agent/agents/reviewer.md").exists());
    run_config_with(
        &client,
        ConfigCommand::Remove {
            path: "agents/reviewer.md".into(),
        },
        &mut io,
    )
    .await
    .expect("remove");
    assert!(!workspace.join(".cookie-agent/agents/reviewer.md").exists());

    // A delegation target no runtime has reported yet applies, with a note.
    let lead = "---\ndescription: Lead\nmode: primary\nenabled: true\nmodels: [{ model: \"openai/gpt-test\" }]\npermissions:\n  delegate: { codex: allow }\n---\nLead.\n";
    io.edits.push_back(lead.to_owned());
    io.output.clear();
    run_config_with(
        &client,
        ConfigCommand::Edit {
            path: Some("agents/lead.md".into()),
        },
        &mut io,
    )
    .await
    .expect("an unknown delegation target applies");
    assert!(
        io.output.iter().any(|line| line
            .starts_with("Note: agent `lead` delegates to `codex`, which is not known yet")),
        "{:?}",
        io.output
    );
    run_config_with(
        &client,
        ConfigCommand::Remove {
            path: "agents/lead.md".into(),
        },
        &mut io,
    )
    .await
    .expect("remove");

    // A hand edit shows in the status until it is reloaded.
    std::fs::write(
        workspace.join(".cookie-agent/config.toml"),
        "[providers]\n[server]\nport = 7500\n",
    )
    .unwrap();
    io.output.clear();
    run_config_with(&client, ConfigCommand::Status, &mut io)
        .await
        .expect("status");
    assert!(
        io.output
            .iter()
            .any(|line| line.contains("cookie config reload")),
        "{:?}",
        io.output
    );
    io.output.clear();
    run_config_with(&client, ConfigCommand::Reload, &mut io)
        .await
        .expect("reload");
    assert!(
        io.output
            .iter()
            .any(|line| line == "Restart the daemon to apply: server."),
        "{:?}",
        io.output
    );
    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
