//! `cookie forward`: list, make, share, and delete a daemon's forwards with
//! the `forward*` methods, so it works against a remote daemon
//! (`docs/site/specs/forwards.md` section 8.3).

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_client_state::{
    forward::{
        ForwardLifetime, daemon_has_forwards, expiry_text, forward_title, status_text, traffic_text,
    },
    shares::{ShareLifetime, link_address},
};
use cookie_agent_protocol::{
    Forward, ForwardCreateParams, ForwardKind, ForwardListen, ForwardShareCreateParams,
    ForwardTarget, ForwardUpdateParams,
};
use cookie_agent_protocol_transport::Client;
use jiff::Timestamp;

use super::memory::MemoryIo;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum ForwardCommand {
    /// List the forwards: handle, kind, title, route, status, address,
    /// expiry, and traffic.
    List,
    /// Serve a machine's web app at the daemon's address, below
    /// `/p/<handle>/`, and print its address. The app shares the web
    /// page's origin, so forward only apps you trust.
    Http {
        /// Where the app is: `<machine>:<port>`, or `<machine>:<host>:<port>`
        /// for a host the machine reaches other than 127.0.0.1.
        target: String,
        #[arg(long)]
        title: Option<String>,
        /// How long it lives: `1h`, `24h`, `7d`, or `never`.
        #[arg(long, default_value = "24h", value_parser = parse_lifetime)]
        expires: ForwardLifetime,
    },
    /// Listen on a TCP port of one machine and carry each connection to
    /// an address another machine reaches.
    Tcp {
        /// Where it listens: `<machine>:<port>`, or `<machine>:<bind>:<port>`
        /// to bind other than 127.0.0.1; port 0 picks a free one.
        from: String,
        /// Where connections go: `<machine>:<port>`, or
        /// `<machine>:<host>:<port>`.
        to: String,
        #[arg(long)]
        title: Option<String>,
        /// How long it lives: `1h`, `24h`, `7d`, or `never`.
        #[arg(long, default_value = "24h", value_parser = parse_lifetime)]
        expires: ForwardLifetime,
    },
    /// As `tcp`, for UDP datagrams.
    Udp {
        /// Where it listens: `<machine>:<port>`, or `<machine>:<bind>:<port>`.
        from: String,
        /// Where datagrams go: `<machine>:<port>`, or
        /// `<machine>:<host>:<port>`.
        to: String,
        #[arg(long)]
        title: Option<String>,
        /// How long it lives: `1h`, `24h`, `7d`, or `never`.
        #[arg(long, default_value = "24h", value_parser = parse_lifetime)]
        expires: ForwardLifetime,
    },
    /// Give a forward a new expiry, counted from now.
    Expires {
        /// Its handle, such as `fw_3f9a1c2e`.
        forward: String,
        /// `1h`, `24h`, `7d`, or `never`.
        #[arg(value_parser = parse_lifetime)]
        expires: ForwardLifetime,
    },
    /// Make a public link to a web forward and print it: anyone with it
    /// reaches the app until the link or the forward ends.
    Share {
        /// Its handle, such as `fw_3f9a1c2e`.
        forward: String,
        /// How long the link works: `1d`, `7d`, `30d`, or `never`.
        #[arg(long, default_value = "7d", value_parser = parse_share_lifetime)]
        expires: ShareLifetime,
    },
    /// Stop and delete a forward; its connections close and its links end.
    Delete {
        /// Its handle, such as `fw_3f9a1c2e`.
        forward: String,
    },
}

fn parse_lifetime(value: &str) -> Result<ForwardLifetime, String> {
    ForwardLifetime::from_code(value).ok_or_else(|| "expected 1h, 24h, 7d, or never".to_owned())
}

fn parse_share_lifetime(value: &str) -> Result<ShareLifetime, String> {
    ShareLifetime::from_code(value).ok_or_else(|| "expected 1d, 7d, 30d, or never".to_owned())
}

fn target(text: &str) -> anyhow::Result<ForwardTarget> {
    cookie_agent_client_state::forward::parse_target(text).map_err(anyhow::Error::msg)
}

fn listen(text: &str) -> anyhow::Result<ForwardListen> {
    cookie_agent_client_state::forward::parse_listen(text).map_err(anyhow::Error::msg)
}

pub(crate) async fn run_forward(
    url: &str,
    token: &str,
    command: ForwardCommand,
) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    super::handshake_ready(&client, super::client_identity("cli")).await?;
    let page = cookie_agent_client_state::shares::page_base(url).unwrap_or_default();
    run_forward_with(&client, &page, command, &mut super::memory::StdioMemoryIo).await
}

/// Where a forward is reached: a web app's URL, or the `bind:port` a TCP or
/// UDP forward listens on.
fn reached(forward: &Forward, page: &str) -> String {
    match (&forward.address, forward.kind) {
        (Some(address), ForwardKind::Http) => format!("{page}{address}"),
        (Some(address), _) => address.clone(),
        (None, _) => "-".to_owned(),
    }
}

/// One line of a forward.
fn forward_line(forward: &Forward, page: &str, now: Timestamp) -> String {
    let title = forward
        .title
        .as_ref()
        .map(|_| format!("  {:?}", forward_title(forward)))
        .unwrap_or_default();
    format!(
        "{}  {}{title}  {}  {}  {}  {}  {}",
        forward.handle,
        forward.kind.as_str(),
        forward.route(),
        status_text(forward),
        reached(forward, page),
        expiry_text(forward, now),
        traffic_text(forward),
    )
}

pub(crate) async fn run_forward_with<I: MemoryIo>(
    client: &Client,
    page: &str,
    command: ForwardCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        daemon_has_forwards(client.daemon_version()),
        "this daemon has no forwards: it needs client protocol 1.109 or later"
    );
    let now = Timestamp::now();
    let create = |kind, to, from, title, expires: ForwardLifetime| ForwardCreateParams {
        kind,
        to,
        from,
        title,
        expires_in_hours: Some(expires.hours()),
    };
    let params = match command {
        ForwardCommand::List => {
            let listed = client
                .list_forwards(None)
                .await
                .context("forwards.list failed")?;
            if listed.forwards.is_empty() {
                return io.write_line("No forwards.");
            }
            for forward in &listed.forwards {
                io.write_line(&forward_line(forward, page, now))?;
            }
            return Ok(());
        }
        ForwardCommand::Http {
            target: to,
            title,
            expires,
        } => create(ForwardKind::Http, target(&to)?, None, title, expires),
        ForwardCommand::Tcp {
            from,
            to,
            title,
            expires,
        } => create(
            ForwardKind::Tcp,
            target(&to)?,
            Some(listen(&from)?),
            title,
            expires,
        ),
        ForwardCommand::Udp {
            from,
            to,
            title,
            expires,
        } => create(
            ForwardKind::Udp,
            target(&to)?,
            Some(listen(&from)?),
            title,
            expires,
        ),
        ForwardCommand::Expires { forward, expires } => {
            let changed = client
                .update_forward(ForwardUpdateParams {
                    forward,
                    title: None,
                    expires_in_hours: Some(expires.hours()),
                })
                .await
                .map_err(anyhow::Error::new)?;
            return io.write_line(&forward_line(&changed.forward, page, now));
        }
        ForwardCommand::Share { forward, expires } => {
            let made = client
                .create_forward_share(ForwardShareCreateParams {
                    forward,
                    expiry: expires.expiry(now),
                    reuse: false,
                })
                .await
                .map_err(anyhow::Error::new)?;
            let ends = made.share.expires_at.map_or_else(
                || "never expires".to_owned(),
                |at| format!("ends {}", cookie_agent_client_state::cron::until(at, now)),
            );
            return io.write_line(&format!(
                "{}  {}  {ends}  {}",
                made.share.id,
                made.share.forward,
                link_address(made.share.url.as_deref(), &made.share.path, page)
            ));
        }
        ForwardCommand::Delete { forward } => {
            client
                .delete_forward(forward.clone())
                .await
                .map_err(anyhow::Error::new)?;
            return io.write_line(&format!("Deleted {forward}."));
        }
    };
    let mut made = client
        .create_forward(params)
        .await
        .map_err(anyhow::Error::new)?
        .forward;
    // The daemon starts it a moment after it stores it: read it again
    // until it is no longer waiting, for at most two seconds.
    for _ in 0..20 {
        if made.status != cookie_agent_protocol::ForwardStatus::Waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let Some(now) = client
            .list_forwards(None)
            .await
            .context("forwards.list failed")?
            .forwards
            .into_iter()
            .find(|forward| forward.handle == made.handle)
        else {
            break;
        };
        made = now;
    }
    io.write_line(&forward_line(&made, page, now))
}
