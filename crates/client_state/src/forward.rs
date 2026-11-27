//! Forwards as every client shows them (`docs/site/specs/forwards.md`
//! section 8): whether the daemon keeps them, a forward's title, status,
//! expiry, and traffic in words, the expiries the form offers, and the
//! forwards of a session's tree.

use cookie_agent_protocol::{Forward, ForwardKind, ForwardStatus, SemVer, SessionId, human_bytes};
use jiff::Timestamp;

use crate::format::relative_time;

/// The client protocol minor from which the daemon keeps forwards.
pub const FORWARDS_MINOR: u32 = 109;

/// Whether a daemon speaking `version` keeps forwards.
pub fn daemon_has_forwards(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= FORWARDS_MINOR)
    })
}

/// Its title, else its route.
pub fn forward_title(forward: &Forward) -> String {
    forward
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| forward.route())
}

/// What it carries, as a tag: `HTTP`, `TCP`, `UDP`.
pub const fn kind_label(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Http => "HTTP",
        ForwardKind::Tcp => "TCP",
        ForwardKind::Udp => "UDP",
    }
}

/// How it is doing, in a few words, with the reason a failure gives.
pub fn status_text(forward: &Forward) -> String {
    let base = match forward.status {
        ForwardStatus::Active => "Active",
        ForwardStatus::Waiting => "Waiting for its machine",
        ForwardStatus::Failed => "Failed",
        ForwardStatus::Expired => "Expired",
    };
    match &forward.reason {
        Some(reason) if !reason.is_empty() => format!("{base}: {reason}"),
        _ => base.to_owned(),
    }
}

/// When it ends: `ends in 23 hours`, `ended 2 hours ago`, `never ends`.
pub fn expiry_text(forward: &Forward, now: Timestamp) -> String {
    match forward.expires_at {
        None => "never ends".to_owned(),
        Some(at) if at <= now => format!("ended {}", relative_time(now, at)),
        Some(at) => format!("ends {}", crate::cron::until(at, now)),
    }
}

/// Its connections and bytes: `2 open · 14 in all · 3.1 MB in · 12 KB out`.
pub fn traffic_text(forward: &Forward) -> String {
    let mut parts = Vec::new();
    if forward.connections_open > 0 {
        parts.push(format!("{} open", forward.connections_open));
    }
    parts.push(match forward.connections_total {
        0 => "no connections yet".to_owned(),
        1 => "1 connection".to_owned(),
        total => format!("{total} connections"),
    });
    if forward.bytes_in + forward.bytes_out > 0 {
        parts.push(format!(
            "{} in · {} out",
            human_bytes(forward.bytes_in),
            human_bytes(forward.bytes_out)
        ));
    }
    parts.join(" · ")
}

/// The expiries the form and *Change…* offer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ForwardLifetime {
    Hour,
    #[default]
    Day,
    Week,
    Never,
}

impl ForwardLifetime {
    pub const ALL: [Self; 4] = [Self::Hour, Self::Day, Self::Week, Self::Never];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Hour => "1 hour",
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Never => "Never",
        }
    }

    /// `expires_in_hours`: `0` for never.
    pub const fn hours(self) -> i64 {
        match self {
            Self::Hour => 1,
            Self::Day => 24,
            Self::Week => 24 * 7,
            Self::Never => 0,
        }
    }

    /// The short form the CLI takes.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Hour => "1h",
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Never => "never",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|lifetime| lifetime.code() == code)
    }
}

/// The forwards session tree `root` made, in the daemon's order.
pub fn tree_forwards(forwards: &[Forward], root: SessionId) -> Vec<Forward> {
    forwards
        .iter()
        .filter(|forward| forward.owner == Some(root))
        .cloned()
        .collect()
}

/// How many forwards are active.
pub fn active_count(forwards: &[Forward]) -> usize {
    forwards
        .iter()
        .filter(|forward| forward.status == ForwardStatus::Active)
        .count()
}
