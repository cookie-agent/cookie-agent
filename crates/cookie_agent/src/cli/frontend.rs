//! Plain `cookie` and `cookie attach`: the TUI against a running daemon, or
//! against one composed in this process.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use cookie_agent_protocol::{Client, validate_websocket_url};

use super::compose::{Runtime, compose, compose_loaded};
use super::runtime::{dial_in, host_runtime_id};
use super::{client_identity, host_name, project};
use crate::local_daemon;

/// The ID a TUI offers its machine's runtime under: `client_` and the host
/// name, sanitized as [`host_runtime_id`] does.
#[cfg(feature = "tui")]
pub(super) fn client_runtime_id(host: &str) -> String {
    host_runtime_id("client", host)
}

/// This process's canonical working directory, which the TUI sends as the
/// directory of the sessions it creates; `None` when it is unavailable.
#[cfg(feature = "tui")]
pub(super) fn client_directory() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cookie_agent_protocol::paths::canonicalize(&cwd).unwrap_or(cwd);
    Some(cwd.to_string_lossy().into_owned())
}

/// Plain `cookie`: the TUI against the daemon `[server]` describes when one
/// answers, else against one composed in this process over `workspace`
/// (always with `standalone`), starting a session in `project` when given.
pub(super) async fn run_plain(
    workspace: &Path,
    standalone: bool,
    project: Option<cookie_agent_protocol::ProjectName>,
) -> anyhow::Result<()> {
    // Syntax highlighting assets load while the runtime composes.
    #[cfg(feature = "tui")]
    cookie_agent_tui::markdown::warm_up_syntax_assets();
    if standalone {
        return run_local_frontend(compose(workspace).await?, Vec::new(), project).await;
    }
    let configuration =
        cookie_agent_config::load().context("load user configuration, agents, and skills")?;
    // One identity for the whole session, as `cookie attach` keeps.
    let identity = client_identity("tui");
    match local_daemon::probe(&configuration.runtime.server, identity.clone()).await {
        local_daemon::Probe::Connected(daemon) => {
            drop(configuration);
            run_daemon_tui(
                daemon.client,
                &daemon.hello,
                identity,
                &daemon.url,
                &daemon.token,
                project,
            )
            .await
        }
        probe => {
            let runtime = compose_loaded(workspace, configuration).await?;
            run_local_frontend(runtime, probe.notice().into_iter().collect(), project).await
        }
    }
}

/// Runs the TUI against the daemon `runtime` composes in this process,
/// showing `notices` from the start.
#[cfg(feature = "tui")]
pub(super) async fn run_local_frontend(
    mut runtime: Runtime,
    notices: Vec<String>,
    project: Option<cookie_agent_protocol::ProjectName>,
) -> anyhow::Result<()> {
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let result = async {
        let client = Client::connect_stream(client_stream);
        client
            .handshake_as(client_identity("tui"))
            .await
            .context("handshake with daemon")?;
        let project = match &project {
            Some(name) => Some(project::find_project(&client, name).await?),
            None => None,
        };
        cookie_agent_tui::run_with_new_session(
            client,
            cookie_agent_tui::SessionPlacement {
                cwd: client_directory(),
                project,
                ..Default::default()
            },
            cookie_agent_tui::ConnectionView {
                label: Some("standalone".into()),
                notices,
            },
        )
        .await
    }
    .await;
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    let server_result = server_task
        .await
        .context("join in-process server task")?
        .context("run in-process server task");
    runtime.engine.shutdown().await;
    result.and(server_result)
}

#[cfg(not(feature = "tui"))]
pub(super) async fn run_local_frontend(
    mut runtime: Runtime,
    _notices: Vec<String>,
    _project: Option<cookie_agent_protocol::ProjectName>,
) -> anyhow::Result<()> {
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
    anyhow::bail!(
        "cookie was built without TUI support; use `cookie daemon`, `cookie connect`, or `cookie disconnect`"
    )
}

/// `cookie attach`: connects to the daemon at `url`, failing rather than
/// running one in this process, and runs the TUI against it.
#[cfg(feature = "tui")]
pub(super) async fn run_attached_tui(url: &str, token: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    // One identity for the whole session: after a reconnect the daemon keeps
    // stamping the same origin, so this TUI still recognizes its own inputs.
    let identity = client_identity("tui");
    let client = Client::connect_websocket_with_token(url, token)
        .await
        .context("connect to daemon WebSocket")?;
    let hello = client
        .handshake_as(identity.clone())
        .await
        .context("handshake with daemon")?;
    run_daemon_tui(client, &hello, identity, url, token, None).await
}

/// Runs the TUI against the daemon `client` reaches at `url`, connected and
/// handshaken as `identity`, as `cookie attach` and plain `cookie` with a
/// running daemon both do: sessions use the runtime
/// [`local_daemon::runtime_for`] chooses, a dropped connection is redialed,
/// and quitting leaves the daemon running.
#[cfg(feature = "tui")]
pub(super) async fn run_daemon_tui(
    client: Client,
    hello: &cookie_agent_protocol::ServerHello,
    identity: cookie_agent_protocol::ClientIdentity,
    url: &str,
    token: &str,
    project: Option<cookie_agent_protocol::ProjectName>,
) -> anyhow::Result<()> {
    let project = match &project {
        Some(name) => Some(project::find_project(&client, name).await?),
        None => None,
    };
    let cwd = client_directory();
    let own_runtime = client_runtime_id(&host_name());
    let choice = local_daemon::runtime_for(
        &client,
        hello,
        cookie_agent_protocol::paths::machine_id().as_deref(),
        &own_runtime,
    )
    .await;
    let (runtime, capability) = match choice {
        local_daemon::RuntimeChoice::Local => (None, None),
        local_daemon::RuntimeChoice::Existing {
            runtime_id,
            capability,
        } => (Some(runtime_id.into_string()), Some(capability)),
        local_daemon::RuntimeChoice::DialIn => {
            start_dial_in(url, token, &own_runtime, cwd.as_deref())?;
            (Some(own_runtime), None)
        }
    };
    let notices = local_daemon::version_notice(
        &hello.engine_version,
        cookie_agent_engine::release::version(),
    )
    .into_iter()
    .collect();
    let address = local_daemon::daemon_address(url);
    let (url, token) = (url.to_owned(), token.to_owned());
    let reconnector: cookie_agent_tui::Reconnector = std::sync::Arc::new(move || {
        let (url, token, identity) = (url.clone(), token.clone(), identity.clone());
        Box::pin(async move {
            let client = Client::connect_websocket_with_token(&url, &token)
                .await
                .map_err(|error| error.to_string())?;
            client
                .handshake_as(identity)
                .await
                .map_err(|error| error.to_string())?;
            Ok(client)
        })
    });
    cookie_agent_tui::run_attached(
        client,
        reconnector,
        cookie_agent_tui::SessionPlacement {
            cwd,
            runtime,
            capability,
            project,
        },
        cookie_agent_tui::ConnectionView {
            label: Some(address),
            notices,
        },
    )
    .await
}

/// Offers the daemon at `url` this machine's directory `cwd` as runtime
/// `id`, dialed in for as long as this process runs: a daemon on another
/// machine cannot see this one.
#[cfg(feature = "tui")]
pub(super) fn start_dial_in(
    url: &str,
    token: &str,
    id: &str,
    cwd: Option<&str>,
) -> anyhow::Result<()> {
    // Its MCP servers and ACP agents are the daemon's `[runtimes.<id>]`.
    let options = cookie_agent_coding_runtime::server::ServeOptions {
        roots: cwd.iter().map(PathBuf::from).collect(),
        ..Default::default()
    };
    let (url, token, id) = (url.to_owned(), token.to_owned(), id.to_owned());
    tokio::spawn(async move {
        // A refusal is reported where this process reports dial-in drops.
        let _ = dial_in(&url, &token, &id, options).await;
    });
    Ok(())
}

#[cfg(not(feature = "tui"))]
pub(super) async fn run_attached_tui(url: &str, _token: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    anyhow::bail!("cookie was built without TUI support")
}

#[cfg(not(feature = "tui"))]
pub(super) async fn run_daemon_tui(
    _client: Client,
    _hello: &cookie_agent_protocol::ServerHello,
    _identity: cookie_agent_protocol::ClientIdentity,
    _url: &str,
    _token: &str,
    _project: Option<cookie_agent_protocol::ProjectName>,
) -> anyhow::Result<()> {
    anyhow::bail!("cookie was built without TUI support")
}
