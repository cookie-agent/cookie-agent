//! `cookie daemon`: serving the composed daemon over WebSocket until a stop
//! signal or a restart, and the tokens it serves with.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use cookie_agent_server::{generate_token, ready_line};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{Launch, daemon_restart};

/// The daemon's tokens: `[server] token`, which clients present, and
/// `[server] runtime_token`, which dial-in runtimes may present instead.
pub(super) struct DaemonTokens {
    pub(super) token: Zeroizing<String>,
    pub(super) runtime_token: Zeroizing<String>,
    /// The token was created by this start, not read from the configuration.
    pub(super) token_created: bool,
    /// The runtime token was created by this start.
    pub(super) runtime_token_created: bool,
}

/// The daemon's tokens from its configuration, with a fresh one written
/// there for each that is missing, so they stay the same across restarts.
pub(super) fn daemon_tokens(
    server: &cookie_agent_config::ServerConfig,
    config_path: Option<&Path>,
) -> anyhow::Result<DaemonTokens> {
    let fresh = |configured: &Option<String>| -> anyhow::Result<Option<Zeroizing<String>>> {
        match configured {
            Some(_) => Ok(None),
            None => generate_token()
                .map(Some)
                .context("generate daemon authentication token"),
        }
    };
    let new_token = fresh(&server.token)?;
    let new_runtime_token = fresh(&server.runtime_token)?;
    if new_token.is_some() || new_runtime_token.is_some() {
        let path = config_path.context("no user config.toml to keep the daemon tokens in")?;
        cookie_agent_config::write_server_tokens(
            path,
            new_token.as_deref().map(String::as_str),
            new_runtime_token.as_deref().map(String::as_str),
        )
        .with_context(|| format!("keep the daemon tokens in {}", path.display()))?;
    }
    let token_created = new_token.is_some();
    let runtime_token_created = new_runtime_token.is_some();
    Ok(DaemonTokens {
        token: new_token
            .or_else(|| server.token.clone().map(Zeroizing::new))
            .context("a daemon token")?,
        runtime_token: new_runtime_token
            .or_else(|| server.runtime_token.clone().map(Zeroizing::new))
            .context("a runtime token")?,
        token_created,
        runtime_token_created,
    })
}

/// What the daemon prints after its ready line about its web page: where
/// it is.
pub(super) fn web_page_line(client_url: &str) -> Option<String> {
    cookie_agent_server::page_url(client_url)
        .map(|page| format!("web page at {page} (`cookie web` prints it with the token)"))
}

/// `cookie daemon`: listens on `port` or `listen`, over TLS with `tls`
/// (certificate, key), then composes the daemon over `workspace` and serves
/// it until a stop signal or a restart, which `launch` then carries out.
/// Clients connect while it composes and see what it is doing
/// (`docs/site/specs/daemon-startup.md`).
pub(super) async fn serve(
    workspace: &Path,
    port: Option<u16>,
    listen: Option<std::net::SocketAddr>,
    tls: Option<(PathBuf, PathBuf)>,
    launch: &mut Launch,
) -> anyhow::Result<()> {
    let tls = match tls {
        Some((cert, key)) => {
            Some(cookie_agent_server::load_tls(&cert, &key).context("load TLS identity")?)
        }
        None => None,
    };
    let load =
        || cookie_agent_config::load().context("load user configuration, agents, and skills");
    let mut configuration = load()?;
    let tokens = daemon_tokens(
        &configuration.runtime.server,
        configuration.config_paths.user.as_deref(),
    )?;
    // The daemon runs on the file as it now stands, so tokens it just wrote
    // are no change waiting to be applied.
    if tokens.token_created || tokens.runtime_token_created {
        configuration = load()?;
    }
    let address = listen.unwrap_or_else(|| {
        std::net::SocketAddr::from((
            [127, 0, 0, 1],
            port.unwrap_or(configuration.runtime.server.port),
        ))
    });
    let exit = run_daemon(
        workspace,
        configuration,
        DaemonListen {
            address,
            tls,
            tokens,
        },
        launch,
    )
    .await?;
    launch.relaunch_now = matches!(exit, DaemonExit::Restart);
    Ok(())
}

/// Where and how `cookie daemon` listens.
pub(super) struct DaemonListen {
    address: std::net::SocketAddr,
    tls: Option<cookie_agent_server::TlsConfig>,
    tokens: DaemonTokens,
}

/// Why the daemon stopped.
pub(super) enum DaemonExit {
    /// A stop signal.
    Stopped,
    /// A client's `daemon.restart`: relaunch.
    Restart,
}

/// How long a daemon whose startup failed waits for the clients connected
/// meanwhile to hear why before it exits.
const FAILED_START_NOTICE: Duration = Duration::from_secs(5);

pub(super) async fn run_daemon(
    workspace: &Path,
    configuration: cookie_agent_config::LoadedConfiguration,
    options: DaemonListen,
    launch: &Launch,
) -> anyhow::Result<DaemonExit> {
    let DaemonListen {
        address,
        tls,
        tokens:
            DaemonTokens {
                token,
                runtime_token,
                ..
            },
    } = options;
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    // A restarted daemon waits for the process it replaces to let go of the
    // port.
    let bind_wait = if launch.restarted {
        daemon_restart::RESTART_BIND_WAIT
    } else {
        Duration::ZERO
    };
    let listener = cookie_agent_server::DaemonListener::new(
        cookie_agent_engine::status::StatusBoard::new(cookie_agent_protocol::DaemonPhase::Starting),
    )
    .with_page_url(configuration.runtime.server.page_url.clone())
    .with_runtime_token(runtime_token.clone())
    .with_share_url(
        configuration
            .runtime
            .public
            .url
            .clone()
            .or_else(|| configuration.runtime.server.page_url.clone()),
    );
    let public_listen = configuration.runtime.public.listen;
    let mut signals =
        cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    let running = daemon_restart::bind_retrying(bind_wait, || {
        listener.serve_on(address, token.clone(), tls.clone())
    })
    .await
    .map_err(|error| anyhow::Error::new(error).context("start WebSocket daemon"))?;
    let url = format!("{scheme}://{}/ws", running.address());
    // Launchers wait for this line: the daemon takes connections from here
    // on, and tells them it is starting until it is ready.
    println!("{}", ready_line(&url));
    println!(
        "cookie daemon listening on {url} (client protocol {}, process {})",
        cookie_agent_protocol::CLIENT_PROTOCOL_VERSION,
        std::process::id()
    );
    if let Some(line) = web_page_line(&url) {
        println!("{line}");
    }
    // The public listener (`[public] listen`): links answer as gone until
    // the daemon is ready.
    let shares = match public_listen {
        Some(address) => match listener.serve_public_on(address).await {
            Ok(shares) => {
                println!("public paths served alone at http://{}/", shares.address());
                Some(shares)
            }
            Err(error) => {
                listener.shutdown();
                running.wait().await;
                return Err(anyhow::Error::new(error)
                    .context(format!("listen for the public paths on {address}")));
            }
        },
        None => None,
    };
    let compose = async {
        #[cfg(debug_assertions)]
        hold_start_for_test(listener.board()).await;
        super::compose::compose_for_daemon(workspace, configuration, &listener).await
    };
    let composed = tokio::select! {
        composed = compose => composed,
        () = signals.recv() => {
            println!("cookie daemon stopped while starting");
            listener.shutdown();
            running.wait().await;
            if let Some(shares) = shares {
                shares.wait().await;
            }
            return Ok(DaemonExit::Stopped);
        }
    };
    let mut runtime = match composed {
        Ok(runtime) => runtime,
        Err(error) => {
            listener
                .fail(&format!("{error:#}"), FAILED_START_NOTICE)
                .await;
            running.wait().await;
            if let Some(shares) = shares {
                shares.wait().await;
            }
            return Err(error);
        }
    };
    drop(token);
    drop(runtime_token);
    // Only a daemon that knows how it was started can start itself again.
    let restart = CancellationToken::new();
    if launch.relaunch.is_some() {
        let restart = restart.clone();
        runtime
            .engine
            .set_restart_hook(Box::new(move || restart.cancel()));
    }
    listener.hand_over(std::sync::Arc::clone(&runtime.server));
    println!("cookie daemon ready");
    // Phones with push notifications hear the daemon is up again.
    runtime.server.announce_started();
    let exit = tokio::select! {
        () = signals.recv() => DaemonExit::Stopped,
        () = restart.cancelled() => DaemonExit::Restart,
    };
    exit_on_next_stop(signals);
    match exit {
        DaemonExit::Stopped => {
            runtime.server.shutdown();
            runtime.stop_catalog_refresh().await;
            running.wait().await;
            if let Some(shares) = shares {
                shares.wait().await;
            }
            runtime.engine.shutdown().await;
        }
        DaemonExit::Restart => {
            println!("cookie daemon restarting");
            // Before anything stops: phones with push notifications hear it,
            // but the one whose page asked.
            runtime.server.announce_restarting().await;
            // The engine stops first, while clients are still connected:
            // they see the interrupted turns end, then the connection close
            // saying the daemon is restarting.
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            runtime.server.shutdown_for_restart();
            // Returns once every client connection sent that close frame
            // (bounded), so the relaunch does not cut it off.
            running.wait().await;
            if let Some(shares) = shares {
                shares.wait().await;
            }
        }
    }
    Ok(exit)
}

/// Holds the start in debug builds, as a blocking task, so a test connects
/// to a daemon that is still starting: for `COOKIE_AGENT_TEST_STARTUP_HOLD_MS`,
/// and while the file `COOKIE_AGENT_TEST_STARTUP_HOLD_FILE` names exists,
/// which a test creates before it restarts the daemon.
#[cfg(debug_assertions)]
async fn hold_start_for_test(board: &cookie_agent_engine::status::StatusBoard) {
    let hold = std::env::var("COOKIE_AGENT_TEST_STARTUP_HOLD_MS")
        .ok()
        .and_then(|millis| millis.parse().ok())
        .map(Duration::from_millis);
    let file = std::env::var_os("COOKIE_AGENT_TEST_STARTUP_HOLD_FILE")
        .map(PathBuf::from)
        .filter(|file| file.exists());
    if hold.is_none() && file.is_none() {
        return;
    }
    let _task = board.start("Holding the start for a test", true);
    if let Some(hold) = hold {
        tokio::time::sleep(hold).await;
    }
    if let Some(file) = file {
        while file.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Exits at once on the next stop signal, for a graceful stop that hangs.
fn exit_on_next_stop(mut signals: cookie_agent_protocol::StopSignals) {
    tokio::spawn(async move {
        signals.recv().await;
        eprintln!("cookie: stopping at once");
        std::process::exit(1);
    });
}

/// Cancels `stop` on the first stop signal; see [`exit_on_next_stop`] for
/// the second.
pub(super) fn stop_on_signal(stop: CancellationToken) -> anyhow::Result<()> {
    let mut signals =
        cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    tokio::spawn(async move {
        signals.recv().await;
        stop.cancel();
        exit_on_next_stop(signals);
    });
    Ok(())
}
