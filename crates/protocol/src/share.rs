//! Share links (`docs/site/specs/share-links.md`, client protocol 1.86.0):
//! public, unguessable URLs that show one asset to anyone who has them,
//! without the daemon token. Conversation shares
//! (`docs/site/specs/conversation-shares.md`, client protocol 1.88.0) are
//! the same kind of URL for one session: whoever opens one is a guest in it.
//!
//! The daemon serves a link's page at `s/<id>` below its share base; clients
//! make, list, change, and revoke links with the methods here and follow
//! `shares.changed`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, AssetId, AssetStore, EventOrigin, SessionId, ShareId};

pub const SHARES_LIST_METHOD: &str = "shares.list";
pub const SHARE_CREATE_METHOD: &str = "share.create";
pub const SHARE_UPDATE_METHOD: &str = "share.update";
pub const SHARE_REVOKE_METHOD: &str = "share.revoke";
pub const SHARES_REVOKE_ALL_METHOD: &str = "shares.revoke_all";
pub const SHARES_CHANGED_METHOD: &str = "shares.changed";
pub const SESSION_SHARE_CREATE_METHOD: &str = "session_share.create";
pub const SESSION_SHARE_UPDATE_METHOD: &str = "session_share.update";

/// The most bytes a conversation share's label has: what fits in the
/// origin of the events its guests cause ([`guest_origin`]).
pub const SHARE_LABEL_MAX_BYTES: usize = 30;
/// The label of a conversation share the user does not name.
pub const SHARE_DEFAULT_LABEL: &str = "Guest";

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

/// One conversation share: a link that lets whoever opens it use one root
/// session as a guest (client protocol 1.88.0).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionShare {
    pub id: ShareId,
    /// The guest page's path relative to the share base, `s/<id>`.
    pub path: String,
    /// The guest page's full URL, as [`Share::url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The root session it opens.
    pub session_id: SessionId,
    /// The session's title now, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Who the link is for; the messages sent through it show it.
    pub label: String,
    /// Whether its guests may act in the session. Off, they only watch.
    pub can_send: bool,
    pub created: ShareCreated,
    /// When the link stops working; never when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<jiff::Timestamp>,
    /// How many guest connections it has had.
    pub views: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed: Option<jiff::Timestamp>,
    /// How many guests are connected now.
    pub connected: u32,
}

/// Lists the links, newest first, with expired ones left out.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharesListParams {
    /// Only the links of this asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<AssetId>,
    /// Only the links of this session (client protocol 1.88.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SharesListResult {
    pub shares: Vec<Share>,
    /// Whether the daemon serves links (`[share] enabled`).
    pub enabled: bool,
    /// The conversation shares (client protocol 1.88.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionShare>,
}

/// Makes a conversation share of a root session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionShareCreateParams {
    pub session_id: SessionId,
    /// Who the link is for; [`SHARE_DEFAULT_LABEL`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = SHARE_LABEL_MAX_BYTES))]
    pub label: Option<String>,
    pub can_send: bool,
    pub expiry: ShareExpiry,
}

/// Changes a conversation share. Its open guest connections follow the new
/// `can_send` at once; a new label applies to guests who connect after it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionShareUpdateParams {
    pub id: ShareId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = SHARE_LABEL_MAX_BYTES))]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_send: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<ShareExpiry>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionShareResult {
    pub share: SessionShare,
}

/// What a guest connection's handshake says about its link (client protocol
/// 1.88.0): a client with this in its hello is a guest in one session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct GuestHello {
    pub session_id: SessionId,
    pub label: String,
    pub can_send: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<jiff::Timestamp>,
}

/// Whether `label` is a label a conversation share may have: on one line,
/// not blank, and at most [`SHARE_LABEL_MAX_BYTES`] bytes.
#[must_use]
pub fn is_share_label(label: &str) -> bool {
    label.len() <= SHARE_LABEL_MAX_BYTES
        && !label.trim().is_empty()
        && !label.chars().any(char::is_control)
}

/// The prefix of a guest's origin below `client:`.
const GUEST_ORIGIN_PREFIX: &str = "guest-";
/// The letters of the origin's encoding of a label: an origin takes only
/// lowercase letters, digits, and dashes.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The origin stamped on what a guest of the link labelled `label` causes:
/// `client:guest-<label in base 32>-<8 hex digits of the connection>`. Every
/// event carries its origin, so the label lives on with the messages after
/// the link is gone, and the label is not the link's secret.
#[must_use]
pub fn guest_origin(label: &str, connection: u32) -> EventOrigin {
    let mut encoded = String::new();
    let (mut bits, mut held) = (0_u32, 0_u32);
    for byte in label.bytes().take(SHARE_LABEL_MAX_BYTES) {
        bits = (bits << 8) | u32::from(byte);
        held += 8;
        while held >= 5 {
            held -= 5;
            encoded.push(char::from(BASE32[((bits >> held) & 31) as usize]));
        }
        bits &= (1 << held) - 1;
    }
    if held > 0 {
        encoded.push(char::from(BASE32[((bits << (5 - held)) & 31) as usize]));
    }
    EventOrigin::new(format!(
        "client:{GUEST_ORIGIN_PREFIX}{encoded}-{connection:08x}"
    ))
    .expect("a label of at most 30 bytes in base 32 forms a valid origin")
}

/// The label of the link a guest's origin came through; `None` for any
/// other origin.
#[must_use]
pub fn guest_label(origin: &EventOrigin) -> Option<String> {
    let slug = origin
        .as_str()
        .strip_prefix("client:")?
        .strip_prefix(GUEST_ORIGIN_PREFIX)?;
    let (encoded, connection) = slug.rsplit_once('-')?;
    if connection.len() != 8 || !connection.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() * 5 / 8);
    let (mut bits, mut held) = (0_u32, 0_u32);
    for letter in encoded.bytes() {
        let value = BASE32.iter().position(|known| *known == letter)?;
        bits = (bits << 5) | u32::try_from(value).ok()?;
        held += 5;
        if held >= 8 {
            held -= 8;
            bytes.push(u8::try_from((bits >> held) & 255).ok()?);
        }
        bits &= (1 << held) - 1;
    }
    String::from_utf8(bytes)
        .ok()
        .filter(|label| is_share_label(label))
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
    fn a_guest_origin_keeps_its_label() {
        for label in [
            "Alice",
            "Guest",
            "a",
            "Zoë · lab",
            &"x".repeat(SHARE_LABEL_MAX_BYTES),
        ] {
            let origin = guest_origin(label, 0xdead_beef);
            assert!(origin.as_str().starts_with("client:guest-"), "{origin:?}");
            assert_eq!(guest_label(&origin).as_deref(), Some(label));
        }
    }

    #[test]
    fn other_origins_have_no_guest_label() {
        for origin in [
            "user",
            "engine:cron",
            "client:web-0123456789abcdef0123456789abcdef",
            "client:guest-notbase32!-00000000",
            "client:guest-ifqwy-short",
        ] {
            let Ok(origin) = EventOrigin::new(origin) else {
                continue;
            };
            assert_eq!(guest_label(&origin), None, "{origin:?}");
        }
    }

    #[test]
    fn a_label_is_one_short_line() {
        assert!(is_share_label("Alice"));
        assert!(is_share_label(&"x".repeat(SHARE_LABEL_MAX_BYTES)));
        assert!(!is_share_label(&"x".repeat(SHARE_LABEL_MAX_BYTES + 1)));
        assert!(!is_share_label("  "));
        assert!(!is_share_label("two\nlines"));
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
