//! `cookie sessions`: converting session logs to this build's history
//! format.

use anyhow::Context as _;
use clap::Subcommand;

use super::data_dir;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum SessionsCommand {
    /// Convert every session to this build's history format now, as
    /// opening each one would, after moving the session store to this
    /// build's layout as a daemon's start does. The originals stay beside
    /// them as `events.jsonl.v<N>.bak`. A session another cookie process
    /// holds is left for it to convert.
    Upgrade,
    /// Print a session's tool groups and skills with their states and who
    /// set them.
    Tools {
        /// The session.
        session_id: cookie_agent_protocol::SessionId,
    },
}

/// `cookie sessions tools <id>` against a daemon.
pub(super) async fn run_session_tools(
    url: &str,
    token: &str,
    session_id: cookie_agent_protocol::SessionId,
) -> anyhow::Result<()> {
    let client =
        cookie_agent_protocol_transport::Client::connect_websocket_with_token(url, token).await?;
    super::handshake_ready(&client, super::client_identity("cli")).await?;
    let tools = client
        .get_session_tools(cookie_agent_protocol::SessionToolsGetParams { session_id })
        .await
        .context("session.tools.get failed")?;
    for line in session_tools_lines(&tools) {
        println!("{line}");
    }
    Ok(())
}

fn layer_word(layer: cookie_agent_protocol::ExposureLayer) -> &'static str {
    match layer {
        cookie_agent_protocol::ExposureLayer::Default => "default",
        cookie_agent_protocol::ExposureLayer::Agent => "agent",
        cookie_agent_protocol::ExposureLayer::Project => "project",
        cookie_agent_protocol::ExposureLayer::Session => "session",
    }
}

/// What `cookie sessions tools` prints: a line per group, then per skill,
/// tab-separated.
pub(super) fn session_tools_lines(
    tools: &cookie_agent_protocol::SessionToolsGetResult,
) -> Vec<String> {
    let mut lines = vec!["GROUP\tSTATE\tSET BY\tTOOLS\tTOKENS".to_owned()];
    for group in &tools.groups {
        let state = if group.hidden_by_permissions {
            "hidden by permissions".to_owned()
        } else {
            match group.state {
                cookie_agent_protocol::ToolGroupState::On => "on",
                cookie_agent_protocol::ToolGroupState::Deferred => "deferred",
                cookie_agent_protocol::ToolGroupState::Off => "off",
            }
            .to_owned()
        };
        lines.push(format!(
            "{}\t{state}\t{}\t{}\t~{}",
            group.selector,
            layer_word(group.layer),
            group.tools.len(),
            group.token_estimate
        ));
    }
    lines.push(String::new());
    lines.push("SKILL\tSTATE\tSET BY".to_owned());
    for skill in tools.skills.iter().filter(|skill| !skill.shadowed) {
        let state = match skill.state {
            cookie_agent_protocol::SkillState::On => "on",
            cookie_agent_protocol::SkillState::Off => "off",
        };
        lines.push(format!(
            "{}\t{state}\t{}",
            skill.name,
            layer_word(skill.layer)
        ));
    }
    lines
}

/// `cookie sessions upgrade`: converts session trees on disk and reports
/// each one that needed it, or could not be converted.
pub(super) fn run_sessions(command: SessionsCommand) -> anyhow::Result<()> {
    let SessionsCommand::Upgrade = command else {
        unreachable!("`cookie sessions tools` asks a daemon");
    };
    let data_root = data_dir()?;
    let upgrades =
        cookie_agent_engine::session::upgrade_sessions(&data_root).context("upgrade sessions")?;
    let (mut current, mut converted, mut in_use, mut failed) = (0, 0, 0, 0);
    for (_, root, upgrade) in &upgrades {
        match upgrade {
            cookie_agent_engine::session::TreeUpgrade::Current => current += 1,
            cookie_agent_engine::session::TreeUpgrade::Converted(conversion) => {
                if !conversion.converted.is_empty() {
                    converted += 1;
                    let formats = conversion
                        .converted
                        .iter()
                        .map(|(_, format)| *format)
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|format| format.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{root}: converted {} log(s) from history format {formats} to {}",
                        conversion.converted.len(),
                        cookie_agent_protocol::HISTORY_FORMAT
                    );
                }
                for (session, format) in &conversion.left {
                    failed += 1;
                    match format {
                        None => println!(
                            "{root}: {session} was written before history formats and is not \
                             converted"
                        ),
                        Some(format) => println!(
                            "{root}: {session} has history format {format}, which this build \
                             cannot convert"
                        ),
                    }
                }
            }
            cookie_agent_engine::session::TreeUpgrade::InUse => {
                in_use += 1;
                println!("{root}: skipped, another cookie process holds it");
            }
            cookie_agent_engine::session::TreeUpgrade::Failed(error) => {
                failed += 1;
                println!(
                    "{root}: not converted: {}",
                    cookie_agent_protocol::diagnostics::detail(&error.to_string())
                );
            }
        }
    }
    println!(
        "{} session tree(s): {converted} converted, {current} already current, {in_use} in use; \
         {failed} session(s) not converted",
        upgrades.len()
    );
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be converted");
    Ok(())
}
