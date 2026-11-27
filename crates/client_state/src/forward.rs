//! Forwards as every client shows them (`docs/site/specs/forwards.md`
//! section 8): whether the daemon keeps them, a forward's title, status,
//! expiry, and traffic in words, the expiries the form offers, and the
//! forwards of a session's tree.

use cookie_agent_protocol::{
    Forward, ForwardCreateParams, ForwardKind, ForwardListen, ForwardStatus, ForwardTarget, SemVer,
    SessionId, human_bytes,
};
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

/// `<machine>:<port>` or `<machine>:<host>:<port>` (a bracketed IPv6 host
/// too) as the machine, the host (127.0.0.1 when omitted), and the port;
/// `what` names it in errors.
pub fn parse_endpoint(text: &str, what: &str) -> Result<(String, String, u16), String> {
    let text = text.trim();
    let Some((machine, rest)) = text.split_once(':') else {
        return Err(format!(
            "{what} is <machine>:<port> or <machine>:<host>:<port>, not {text:?}"
        ));
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host.trim_start_matches('[').trim_end_matches(']'), port),
        None => ("127.0.0.1", rest),
    };
    let port = port
        .trim()
        .parse::<u16>()
        .map_err(|_| format!("{what}'s port is a number from 0 to 65535, not {port:?}"))?;
    if machine.trim().is_empty() {
        return Err(format!("{what} names no machine"));
    }
    if host.trim().is_empty() {
        return Err(format!("{what} names no host"));
    }
    Ok((machine.trim().to_owned(), host.trim().to_owned(), port))
}

/// Where a forward's connections go, from `<machine>:[<host>:]<port>`.
pub fn parse_target(text: &str) -> Result<ForwardTarget, String> {
    let (machine, host, port) = parse_endpoint(text, "the target")?;
    if port == 0 {
        return Err("the target's port is 0".to_owned());
    }
    Ok(ForwardTarget {
        machine,
        host,
        port,
    })
}

/// Where a TCP or UDP forward listens, from `<machine>:[<bind>:]<port>`.
pub fn parse_listen(text: &str) -> Result<ForwardListen, String> {
    let (machine, bind, port) = parse_endpoint(text, "where it listens")?;
    Ok(ForwardListen {
        machine,
        bind,
        port,
    })
}

/// The New forward form as the TUI's editor shows it, its target on
/// `machine` when one is known.
pub fn form_text(machine: Option<&str>) -> String {
    format!(
        "# A new forward: save and close to make it, or empty the file to drop it.\n\
         # Lines starting with # are ignored.\n\
         #\n\
         # kind: http (a web app at the daemon's address), tcp, or udp\n\
         kind: http\n\
         # to: where it goes, reached from its machine:\n\
         #   <machine>:<port>, or <machine>:<host>:<port>\n\
         to: {}:\n\
         # from: where a tcp or udp forward listens: <machine>:<port>, or\n\
         #   <machine>:<bind>:<port>; port 0 picks a free one\n\
         from:\n\
         title:\n\
         # expires: 1h, 24h, 7d, or never\n\
         expires: 24h\n",
        machine.unwrap_or("")
    )
}

/// The New forward form `text` as `forward.create` takes it.
pub fn parse_form(text: &str) -> Result<ForwardCreateParams, String> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("{line:?} is not `key: value`"));
        };
        let key = key.trim().to_ascii_lowercase();
        if !matches!(key.as_str(), "kind" | "to" | "from" | "title" | "expires") {
            return Err(format!("unknown field {key:?}"));
        }
        fields.insert(key, value.trim().to_owned());
    }
    let field = |key: &str| {
        fields
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    let kind = match field("kind").unwrap_or("http") {
        "http" => ForwardKind::Http,
        "tcp" => ForwardKind::Tcp,
        "udp" => ForwardKind::Udp,
        other => return Err(format!("kind is http, tcp, or udp, not {other:?}")),
    };
    let to = parse_target(field("to").ok_or("name where it goes in `to`")?)?;
    let from = match (kind, field("from")) {
        (ForwardKind::Http, None) => None,
        (ForwardKind::Http, Some(_)) => {
            return Err(
                "an http forward is served at the daemon's address: leave `from` empty".to_owned(),
            );
        }
        (_, Some(from)) => Some(parse_listen(from)?),
        (_, None) => {
            return Err(format!(
                "a {} forward listens somewhere: fill in `from`",
                kind.as_str()
            ));
        }
    };
    let expires = match field("expires") {
        None => ForwardLifetime::default(),
        Some(code) => ForwardLifetime::from_code(code)
            .ok_or_else(|| format!("expires is 1h, 24h, 7d, or never, not {code:?}"))?,
    };
    Ok(ForwardCreateParams {
        kind,
        to,
        from,
        title: field("title").map(str::to_owned),
        expires_in_hours: Some(expires.hours()),
    })
}
