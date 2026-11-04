//! `cookie share`: list, make, and revoke a daemon's share links with the
//! `share.*` methods, so it works against a remote daemon
//! (`docs/site/specs/share-links.md` section 7.1).

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_client_state::shares::{ShareLifetime, daemon_has_shares, share_address};
use cookie_agent_protocol::{
    Asset, AssetStore, AssetsSearchParams, ProjectId, SessionId, Share, ShareCreateParams, ShareId,
    SharesListParams,
};
use cookie_agent_protocol_transport::Client;
use jiff::{Timestamp, tz::TimeZone};

use super::memory::MemoryIo;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum ShareCommand {
    /// List the live links: ID, title, expiry, views, and address.
    List,
    /// Make a link to an asset and print its address. An asset that
    /// already has a live link prints that one, unless `--new`.
    Create {
        /// The asset's handle, such as `asset_3f9a1c2e`, or its UUID.
        asset: String,
        /// Look for the handle among this session tree's own assets.
        #[arg(long, value_name = "SESSION_ID", conflicts_with = "project")]
        session: Option<SessionId>,
        /// Look for the handle among this project's assets.
        #[arg(long, value_name = "PROJECT_ID")]
        project: Option<ProjectId>,
        /// How long the link works: `1d`, `7d`, `30d`, or `never`.
        #[arg(long, default_value = "7d", value_parser = parse_lifetime)]
        expires: ShareLifetime,
        /// Make a new link even when the asset has one.
        #[arg(long)]
        new: bool,
    },
    /// Revoke a link, or every link with `--all`; it stops working at
    /// once.
    Revoke {
        #[arg(value_parser = parse_share_id, required_unless_present = "all")]
        id: Option<ShareId>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
}

fn parse_lifetime(value: &str) -> Result<ShareLifetime, String> {
    ShareLifetime::from_code(value).ok_or_else(|| "expected 1d, 7d, 30d, or never".to_owned())
}

fn parse_share_id(value: &str) -> Result<ShareId, String> {
    ShareId::new(value).map_err(|error| error.to_string())
}

pub(crate) async fn run_share(url: &str, token: &str, command: ShareCommand) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    super::handshake_ready(&client, super::client_identity("cli")).await?;
    let page = cookie_agent_client_state::shares::page_base(url).unwrap_or_default();
    run_share_with(&client, &page, command, &mut super::memory::StdioMemoryIo).await
}

/// One line of a link: its ID, title, expiry, views, and address.
fn share_line(share: &Share, page: &str, zone: &TimeZone) -> String {
    let expiry = share.expires_at.map_or_else(
        || "never expires".to_owned(),
        |at| {
            format!(
                "expires {}",
                at.to_zoned(zone.clone()).strftime("%Y-%m-%d %H:%M")
            )
        },
    );
    format!(
        "{}  {:?}  {expiry}  {} view{}  {}",
        share.id,
        share.title,
        share.views,
        if share.views == 1 { "" } else { "s" },
        share_address(share, page)
    )
}

/// The asset `reference` names, by handle or UUID, among the assets of
/// `store` or, without one, of every store.
async fn find_asset(
    client: &Client,
    reference: &str,
    store: Option<&AssetStore>,
) -> anyhow::Result<Asset> {
    let assets = match store {
        Some(store) => {
            client
                .list_assets(store.clone())
                .await
                .context("assets.list failed")?
                .assets
        }
        None => {
            client
                .search_assets(AssetsSearchParams::default())
                .await
                .context("assets.search failed")?
                .assets
        }
    };
    let matches = assets
        .into_iter()
        .filter(|asset| asset.handle.as_str() == reference || asset.id.to_string() == reference)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [asset] => Ok(asset.clone()),
        [] => anyhow::bail!("there is no asset {reference:?}"),
        several => anyhow::bail!(
            "{reference:?} names {} assets; pick one with --session or --project:\n{}",
            several.len(),
            several
                .iter()
                .map(|asset| format!("  {} in {:?}", asset.summary(), asset.store))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

pub(crate) async fn run_share_with<I: MemoryIo>(
    client: &Client,
    page: &str,
    command: ShareCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        daemon_has_shares(client.daemon_version()),
        "this daemon has no share links: it needs client protocol 1.86 or later"
    );
    let zone = TimeZone::system();
    match command {
        ShareCommand::List => {
            let listed = client
                .list_shares(SharesListParams::default())
                .await
                .context("shares.list failed")?;
            if !listed.enabled {
                io.write_line("Share links are off ([share] enabled = false): no link works.")?;
            }
            if listed.shares.is_empty() {
                return io.write_line("No share links.");
            }
            for share in &listed.shares {
                io.write_line(&share_line(share, page, &zone))?;
            }
            Ok(())
        }
        ShareCommand::Create {
            asset,
            session,
            project,
            expires,
            new,
        } => {
            let store = match (session, project) {
                (Some(session), _) => Some(AssetStore::Session { session }),
                (None, Some(project)) => Some(AssetStore::Project { project }),
                (None, None) => None,
            };
            let asset = find_asset(client, asset.trim(), store.as_ref()).await?;
            let made = client
                .create_share(ShareCreateParams {
                    store: asset.store.clone(),
                    asset: asset.id,
                    expiry: expires.expiry(Timestamp::now()),
                    reuse: !new,
                })
                .await
                .map_err(anyhow::Error::new)?;
            if !made.created {
                io.write_line(&format!(
                    "{} already has a live link (pass --new for another):",
                    asset.handle
                ))?;
            }
            io.write_line(&share_line(&made.share, page, &zone))
        }
        ShareCommand::Revoke { id: Some(id), .. } => {
            client
                .revoke_share(id.clone())
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!("Revoked {id}."))
        }
        ShareCommand::Revoke { id: None, .. } => {
            let revoked = client
                .revoke_all_shares()
                .await
                .map_err(anyhow::Error::new)?
                .revoked;
            io.write_line(&format!(
                "Revoked {revoked} link{}.",
                if revoked == 1 { "" } else { "s" }
            ))
        }
    }
}
