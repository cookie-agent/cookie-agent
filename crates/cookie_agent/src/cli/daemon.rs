//! `cookie daemon`: serving the composed daemon over WebSocket until a stop
//! signal or a restart, and the token it serves with.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use cookie_agent_server::{generate_token, ready_line};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::compose::{Runtime, compose};
use crate::{Launch, daemon_restart};

/// The daemon's token: `[server] token` from its configuration, or a fresh
/// one written there when there is none, so it stays the same across
/// restarts.
pub(super) fn daemon_token(
    configured: Option<String>,
    config_path: Option<&Path>,
) -> anyhow::Result<Zeroizing<String>> {
    if let Some(token) = configured {
        return Ok(Zeroizing::new(token));
    }
    let token = generate_token().context("generate daemon authentication token")?;
    let path = config_path.context("no user config.toml to keep the daemon token in")?;
    cookie_agent_config::write_server_token(path, &token)
        .with_context(|| format!("keep the daemon token in {}", path.display()))?;
    Ok(token)
}

/// What the daemon prints after its ready line about its web page: where
/// it is.
pub(super) fn web_page_line(client_url: &str) -> Option<String> {
    cookie_agent_server::page_url(client_url)
        .map(|page| format!("web page at {page} (`cookie web` prints it with the token)"))
}

/// `cookie daemon`: composes the daemon over `workspace` and serves it on
/// `port` or `listen`, over TLS with `tls` (certificate, key), until a stop
/// signal or a restart, which `launch` then carries out.
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
    let runtime = compose(workspace).await?;
    let token = daemon_token(runtime.token.clone(), runtime.config_path.as_deref())?;
    if runtime.token.is_none() {
        runtime.engine.note_served_server_token(token.as_str());
    }
    let exit = run_daemon(
        runtime,
        DaemonListen {
            port,
            listen,
            tls,
            token,
        },
        launch,
    )
    .await?;
    launch.relaunch_now = matches!(exit, DaemonExit::Restart);
    Ok(())
}

/// Where and how `cookie daemon` listens.
pub(super) struct DaemonListen {
    port: Option<u16>,
    listen: Option<std::net::SocketAddr>,
    tls: Option<cookie_agent_server::TlsConfig>,
    token: Zeroizing<String>,
}

/// Why the daemon stopped.
pub(super) enum DaemonExit {
    /// A stop signal.
    Stopped,
    /// A client's `daemon.restart`: relaunch.
    Restart,
}

pub(super) async fn run_daemon(
    mut runtime: Runtime,
    options: DaemonListen,
    launch: &Launch,
) -> anyhow::Result<DaemonExit> {
    let DaemonListen {
        port,
        listen,
        tls,
        token,
    } = options;
    let address = listen.unwrap_or_else(|| {
        std::net::SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(runtime.port)))
    });
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    // A restarted daemon waits for the process it replaces to let go of the
    // port.
    let bind_wait = if launch.restarted {
        daemon_restart::RESTART_BIND_WAIT
    } else {
        Duration::ZERO
    };
    let started = match cookie_agent_protocol::StopSignals::new() {
        Ok(signals) => daemon_restart::bind_retrying(bind_wait, || {
            runtime
                .server
                .clone()
                .serve_on(address, token.clone(), tls.clone())
        })
        .await
        .map(|listener| (signals, listener))
        .map_err(|error| anyhow::Error::new(error).context("start WebSocket daemon")),
        Err(error) => Err(anyhow::Error::new(error).context("listen for stop signals")),
    };
    let (mut signals, listener) = match started {
        Ok(started) => started,
        Err(error) => {
            runtime.server.shutdown();
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            return Err(error);
        }
    };
    // Only a daemon that knows how it was started can start itself again.
    let restart = CancellationToken::new();
    if launch.relaunch.is_some() {
        let restart = restart.clone();
        runtime
            .engine
            .set_restart_hook(Box::new(move || restart.cancel()));
    }
    let url = format!("{scheme}://{}/ws", listener.address());
    println!("{}", ready_line(&url));
    drop(token);
    println!(
        "cookie daemon listening on {url} (client protocol {}, process {})",
        cookie_agent_protocol::CLIENT_PROTOCOL_VERSION,
        std::process::id()
    );
    if let Some(line) = web_page_line(&url) {
        println!("{line}");
    }
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
            listener.wait().await;
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
            listener.wait().await;
        }
    }
    Ok(exit)
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
