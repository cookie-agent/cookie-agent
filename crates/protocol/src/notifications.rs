//! Push notifications (`docs/site/specs/push-notifications.md`): the
//! browsers the daemon sends Web Push messages to when a root session
//! settles or needs the user, and what a client shows the user now, so
//! the daemon does not push what is on screen. Client protocol 1.30.0.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SessionId;

pub const NOTIFICATIONS_GET_METHOD: &str = "notifications.get";
pub const NOTIFICATIONS_SUBSCRIBE_METHOD: &str = "notifications.subscribe";
pub const NOTIFICATIONS_UNSUBSCRIBE_METHOD: &str = "notifications.unsubscribe";
pub const NOTIFICATIONS_TEST_METHOD: &str = "notifications.test";
pub const NOTIFICATIONS_CHANGED_METHOD: &str = "notifications.changed";
pub const CLIENT_PRESENCE_METHOD: &str = "client.presence";
pub const SESSION_UNREAD_SET_METHOD: &str = "session.unread.set";

/// The most sessions one `session.unread.set` marks.
pub const MAX_UNREAD_SET_SESSIONS: usize = 256;

/// The most characters a subscription's label may have.
pub const PUSH_LABEL_MAX_CHARS: usize = 80;
/// The longest endpoint URL the daemon stores.
pub const PUSH_ENDPOINT_MAX_BYTES: usize = 2048;

/// A browser the daemon pushes to, as clients see it: never its endpoint
/// or keys.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PushSubscription {
    /// Eight lowercase letters and digits, assigned by the daemon.
    pub id: String,
    pub label: String,
    /// The host of the push service the endpoint points at, such as
    /// `fcm.googleapis.com`.
    pub push_service: String,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_at: Option<Timestamp>,
    /// Why the last send failed, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsGetParams {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NotificationsGetResult {
    /// The daemon's VAPID public key, an uncompressed P-256 point in
    /// base64url: the `applicationServerKey` browsers subscribe with.
    pub public_key: String,
    /// Whether the daemon sends pushes: `[notifications] enabled`.
    pub enabled: bool,
    pub subscriptions: Vec<PushSubscription>,
}

/// Registers a browser's push subscription, or updates the one with the
/// same endpoint, which keeps its id.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsSubscribeParams {
    /// The subscription's `https://` endpoint.
    pub endpoint: String,
    /// The subscription's `p256dh` key, base64url.
    pub p256dh: String,
    /// The subscription's `auth` secret, base64url.
    pub auth: String,
    /// 1 to 80 characters.
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NotificationsSubscribeResult {
    pub subscription: PushSubscription,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsUnsubscribeParams {
    pub id: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NotificationsUnsubscribeResult {}

/// Sends a test push to one subscription now, whatever presence and
/// `[notifications] enabled` say.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsTestParams {
    pub id: String,
}

crate::open_enum! {
    /// What the push service did with a test push.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum NotificationsTestStatus {
        /// The push service took it.
        Sent,
        /// The subscription no longer exists; the daemon removed it.
        Gone,
        /// The push service refused it, or could not be reached.
        Failed,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NotificationsTestResult {
    pub status: NotificationsTestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The subscriptions changed; refetch `notifications.get`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NotificationsChangedNotification {}

/// What this connection shows the user now: a session the user is looking
/// at, or `null` when the user is not there (the window is hidden or
/// idle). It lasts until the next call or the connection's end.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPresenceParams {
    pub session: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ClientPresenceResult {}

/// Marks the root sessions `session_ids` (1 to
/// [`MAX_UNREAD_SET_SESSIONS`]) unread or read (client protocol 1.51.0,
/// `docs/site/specs/unread-sessions.md` section 5).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnreadSetParams {
    pub session_ids: Vec<SessionId>,
    pub unread: bool,
}

/// What `session.unread.set` did: the sessions whose state changed, each
/// also announced by `sessions.changed`, and the IDs it left.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionUnreadSetResult {
    pub changed: Vec<SessionId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SessionUnreadSkip>,
}

crate::open_enum! {
    /// Why `session.unread.set` left a session.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SessionUnreadSkipReason {
        SessionNotFound,
        /// A delegated session: its root is the one marked.
        NotARootSession,
    }
}

/// A session `session.unread.set` left, and why.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionUnreadSkip {
    pub session_id: SessionId,
    pub reason: SessionUnreadSkipReason,
    /// For `not_a_root_session`, its root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<SessionId>,
}
