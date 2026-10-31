//! `cookie mcp`: listing MCP servers and authorizing a remote one over
//! OAuth.

use std::time::Duration;

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::oauth_callback::OAuthCallbackListener;
use cookie_agent_protocol::{
    Client, McpAuthBeginParams, McpAuthCancelParams, McpAuthCompleteParams, McpServerState,
};

use super::client_identity;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum McpCommand {
    /// List configured MCP servers.
    List,
    /// Start OAuth authorization for a remote MCP server.
    Auth { server: String },
}

pub(super) async fn run_mcp(url: &str, token: &str, command: McpCommand) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    match command {
        McpCommand::List => {
            let servers = client
                .list_mcp_servers()
                .await
                .context("mcp.server.list failed")?
                .servers;
            if servers.is_empty() {
                println!("No MCP servers are configured.");
            }
            for server in servers {
                let runtime = match &server.workspace {
                    Some(workspace) => format!("{} at {workspace}", server.runtime_id),
                    None => server.runtime_id.to_string(),
                };
                println!(
                    "{}\t{} tools\t{:?}\t{runtime}",
                    server.name, server.tool_count, server.state
                );
            }
            Ok(())
        }
        McpCommand::Auth { server } => {
            let status = client
                .list_mcp_servers()
                .await
                .context("mcp.server.list failed")?
                .servers
                .into_iter()
                .find(|candidate| {
                    candidate.name == server
                        && candidate.runtime_id.as_str() == cookie_agent_protocol::RuntimeId::GLOBAL
                })
                .with_context(|| format!("unknown MCP server `{server}`"))?;
            if status.state != McpServerState::NeedsAuth {
                anyhow::bail!(
                    "MCP server `{server}` does not currently require OAuth authorization"
                );
            }
            authorize_mcp_server(&client, server).await
        }
    }
}

/// Waits for the browser's redirect on this machine's loopback, or for the
/// redirect URL pasted on stdin when the browser runs elsewhere, and hands it
/// to the daemon, which exchanges the code.
async fn authorize_mcp_server(client: &Client, server: String) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt as _;

    let listener = OAuthCallbackListener::bind()
        .await
        .context("listen for the OAuth callback")?;
    let redirect_uri = listener.redirect_uri();
    let result = client
        .begin_mcp_auth(McpAuthBeginParams {
            server: server.clone(),
            redirect_uri: redirect_uri.clone(),
        })
        .await
        .context("mcp.auth.begin failed")?;
    println!("Open this URL to authenticate MCP server `{server}`:");
    println!("{}", result.authorization_url);
    println!();
    println!("Waiting for the browser to return to {redirect_uri}.");
    println!("If the browser runs on another machine, paste the URL it was redirected to.");

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let mut status_poll = tokio::time::interval(Duration::from_secs(2));
    let mut signals =
        cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    loop {
        let completed = tokio::select! {
            callback = listener.accept() => {
                let callback = callback.context("receive the OAuth callback")?;
                let completed = complete_mcp_auth(client, &server, callback.url()).await;
                let _ = callback.respond(completed.is_ok()).await;
                completed
            }
            line = stdin.next_line(), if stdin_open => match line.context("read stdin")? {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => complete_mcp_auth(client, &server, &line).await,
                None => {
                    stdin_open = false;
                    continue;
                }
            },
            _ = status_poll.tick() => {
                if let Some(message) = ended_mcp_auth(client, &server).await? {
                    anyhow::bail!("MCP authorization for `{server}` ended: {message}");
                }
                continue;
            }
            () = signals.recv() => {
                let _ = client
                    .cancel_mcp_auth(McpAuthCancelParams { server: server.clone() })
                    .await;
                anyhow::bail!("MCP authorization for `{server}` cancelled");
            }
        };
        match completed {
            Ok(()) => {
                println!("MCP server `{server}` authorized.");
                return Ok(());
            }
            // A URL that is not this flow's callback leaves it waiting.
            Err(error) => match ended_mcp_auth(client, &server).await? {
                Some(_) => return Err(error),
                None => eprintln!("{error:#}"),
            },
        }
    }
}

async fn complete_mcp_auth(
    client: &Client,
    server: &str,
    callback_url: &str,
) -> anyhow::Result<()> {
    client
        .complete_mcp_auth(McpAuthCompleteParams {
            server: server.to_owned(),
            callback_url: callback_url.trim().to_owned(),
        })
        .await
        .context("mcp.auth.complete failed")?;
    Ok(())
}

/// The server's status message once its authorization is no longer waiting.
async fn ended_mcp_auth(client: &Client, server: &str) -> anyhow::Result<Option<String>> {
    let status = client
        .list_mcp_servers()
        .await
        .context("mcp.server.list failed")?
        .servers
        .into_iter()
        .find(|candidate| candidate.name == server)
        .with_context(|| format!("unknown MCP server `{server}`"))?;
    Ok((status.auth_in_progress != Some(true)).then(|| {
        status
            .message
            .unwrap_or_else(|| "no longer in progress".into())
    }))
}
