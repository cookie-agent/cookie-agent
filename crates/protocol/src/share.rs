//! Share links (`docs/site/specs/share-links.md`, client protocol 1.86.0):
//! public, unguessable URLs that show one asset to anyone who has them,
//! without the daemon token.
//!
//! The daemon serves a link's landing page at `s/<id>` below its share
//! base; clients make, list, change, and revoke links with the methods
//! here and follow `shares.changed`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, AssetId, AssetStore, SessionId, ShareId};

pub const SHARES_LIST_METHOD: &str = "shares.list";
pub const SHARE_CREATE_METHOD: &str = "share.create";
pub const SHARE_UPDATE_METHOD: &str = "share.update";
pub const SHARE_REVOKE_METHOD: &str = "share.revoke";
pub const SHARES_REVOKE_ALL_METHOD: &str = "shares.revoke_all";
pub const SHARES_CHANGED_METHOD: &str = "shares.changed";

/// The path below the share base that a link's landing page is at, with
/// the ID after it.
pub const SHARE_PATH_PREFIX: &str = "s/";

/// One share link (section 3).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Share {
    pub id: ShareId,
    /// The landing page's path relative to the share base, `s/<id>`.
    pub path: String,
    /// The landing page's full URL, when the daemon knows its share base
    /// (`[share] url` or `[server] page_url`). Absent, a client puts the
    /// address it reached the daemon's page at before `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The asset it shows, always as the asset is now.
    pub store: AssetStore,
    pub asset: AssetId,
    /// What the landing page is headed with; the asset's title when the
    /// link was made, unless changed since.
    pub title: String,
    /// The asset's current type and size.
    pub media_type: String,
    pub byte_length: u64,
    pub created: ShareCreated,
    /// When the link stops working; never when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<jiff::Timestamp>,
    /// How many times the landing page was opened. Approximate: chat apps
    /// that preview links open it too.
    pub views: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed: Option<jiff::Timestamp>,
}

/// When a link was made, and by whom: the user when `session` is absent,
/// else an agent's call in that session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ShareCreated {
    pub at: jiff::Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// When a link stops working.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShareExpiry {
    Never,
    At { at: jiff::Timestamp },
}

impl ShareExpiry {
    /// The time, `None` for never.
    #[must_use]
    pub const fn at(&self) -> Option<jiff::Timestamp> {
        match self {
            Self::Never => None,
            Self::At { at } => Some(*at),
        }
    }
}

/// Lists the links, newest first, with expired ones left out.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharesListParams {
    /// Only the links of this asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<AssetId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SharesListResult {
    pub shares: Vec<Share>,
    /// Whether the daemon serves links (`[share] enabled`).
    pub enabled: bool,
}

/// Makes a link to an asset, or with `reuse` returns the newest live link
/// the asset already has.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShareCreateParams {
    pub store: AssetStore,
    pub asset: AssetId,
    pub expiry: ShareExpiry,
    #[serde(default)]
    pub reuse: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ShareCreateResult {
    pub share: Share,
    /// Whether the link is new; false when `reuse` returned one.
    pub created: bool,
}

/// Changes a link's title or expiry.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShareUpdateParams {
    pub id: ShareId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = crate::ASSET_TITLE_MAX_CHARS))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<ShareExpiry>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ShareUpdateResult {
    pub share: Share,
}

/// Revokes a link: it stops working at once.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRevokeParams {
    pub id: ShareId,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ShareRevokeResult {}

/// Revokes every link.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharesRevokeAllParams {}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SharesRevokeAllResult {
    pub revoked: u64,
}

/// A link was made, changed, revoked, or removed with its asset; refetch
/// `shares.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SharesChangedNotification {}

/// The landing page URL of the link `id` under `base`, a share base such
/// as `https://share.example.com` or `http://127.0.0.1:7419/cookie/`.
#[must_use]
pub fn share_url(base: &str, id: &ShareId) -> String {
    format!("{}/{SHARE_PATH_PREFIX}{id}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ids_are_22_base64url_characters() {
        assert!(ShareId::new("q3Vt0x9LmA2bCdEfGh1i-_").is_ok());
        for invalid in [
            "q3Vt0x9LmA2bCdEfGh1i-",
            "q3Vt0x9LmA2bCdEfGh1i-__",
            "q3Vt0x9LmA2bCdEfGh1i+/",
            "",
        ] {
            assert!(ShareId::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn an_expiry_is_tagged() {
        assert_eq!(
            serde_json::to_value(ShareExpiry::Never).unwrap(),
            json!({"type": "never"})
        );
        let at: jiff::Timestamp = "2026-11-10T00:00:00Z".parse().unwrap();
        assert_eq!(
            serde_json::from_value::<ShareExpiry>(json!({"type": "at", "at": at})).unwrap(),
            ShareExpiry::At { at }
        );
    }

    #[test]
    fn a_url_joins_the_base_and_the_path() {
        let id = ShareId::new("q3Vt0x9LmA2bCdEfGh1iJk").unwrap();
        assert_eq!(
            share_url("https://share.example.com", &id),
            "https://share.example.com/s/q3Vt0x9LmA2bCdEfGh1iJk"
        );
        assert_eq!(
            share_url("http://127.0.0.1:7419/cookie/", &id),
            "http://127.0.0.1:7419/cookie/s/q3Vt0x9LmA2bCdEfGh1iJk"
        );
    }
}
